//! Relational mail transactions, immutable body BLOBs and WAL snapshots.
use super::LockedRoot;
use crate::{
    format::{
        self,
        key::Key,
        operation::{Operation, Value},
        row::Row,
        ObjectType, Sequence, Table,
    },
    ids::{AccountId, BlobId, StoreEpoch},
    ports::{
        self, Change, ChangeAction, ChangeCursor, ChangeRecord, ChangeStep, Clock, Deadline,
        Digest, Mutation, ReadView, Record, ViewIdentity,
    },
    row_references::ReferenceCheck,
    store_paths::{Name, RootEntry},
};
use rusqlite::{params, types::ValueRef, Connection, OpenFlags};
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::{Arc, Mutex, MutexGuard, TryLockError},
};

use crate::limits::{
    SQLITE_MAX_PAGES as MAX_PAGES, SQLITE_PAGE_BYTES as PAGE_BYTES,
    SQLITE_TRANSACTION_WAL_BYTES as TRANSACTION_WAL_BYTES, SQLITE_WAL_BYTES as MAX_WAL_BYTES,
};
const MAX_OPERATIONS: usize = 4096;
const MAX_TRANSACTION_BYTES: usize = 1_048_576;
const VM_STEPS: u64 = 8_000_000;
const INTEGRITY_VM_STEPS: u64 = MAX_PAGES * PAGE_BYTES * 128;
const MAX_SHM_BYTES: u64 = crate::limits::SQLITE_WAL_INDEX_BYTES as u64;
const MAX_BODY_BYTES: u64 = crate::limits::MAX_MESSAGE_BYTES as u64;
const APP_ID: i64 = 0x54444d41;
const NEXT_CHANGE: &str = "SELECT sequence,operation,action,object FROM changes INDEXED BY changes_kind WHERE account=?1 AND kind=?2 AND (sequence>?3 OR (sequence=?3 AND operation>?4)) AND sequence<=?5 ORDER BY sequence,operation LIMIT 1";
#[path = "index/relational.rs"]
mod relational;
use relational::SCHEMA;
#[cfg(test)]
#[path = "index/crash_tests.rs"]
mod crash_tests;

/// Borrowed stream for one new body. Its claimed length and digest come from
/// the transaction's BlobRow and are independently checked before commit.
pub struct BlobSource<'a> {
    pub id: BlobId,
    pub source: &'a mut dyn std::io::Read,
}

fn lock<T>(value: &Mutex<T>) -> Result<MutexGuard<'_, T>, ports::Error> {
    value.lock().map_err(|_| ports::Error::WriterStopped)
}
fn sql(error: rusqlite::Error) -> ports::Error {
    match error {
        rusqlite::Error::SqliteFailure(e, _) => match e.code {
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                ports::Error::Busy
            }
            rusqlite::ErrorCode::OutOfMemory
            | rusqlite::ErrorCode::DiskFull
            | rusqlite::ErrorCode::TooBig => ports::Error::Capacity,
            rusqlite::ErrorCode::ConstraintViolation => ports::Error::Conflict,
            rusqlite::ErrorCode::OperationInterrupted => ports::Error::Deadline,
            rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase => {
                ports::Error::Corrupt
            }
            rusqlite::ErrorCode::ApiMisuse
            | rusqlite::ErrorCode::TypeMismatch
            | rusqlite::ErrorCode::ParameterOutOfRange => ports::Error::Invalid,
            _ => ports::Error::Io {
                kind: std::io::ErrorKind::Other,
                os_code: None,
            },
        },
        _ => ports::Error::Corrupt,
    }
}
fn sequence(bytes: &[u8]) -> Result<Sequence, ports::Error> {
    Ok(Sequence::from_u64(u64::from_be_bytes(
        bytes.try_into().map_err(|_| ports::Error::Corrupt)?,
    )))
}
fn copy_blob(
    row: &rusqlite::Row<'_>,
    column: usize,
    output: &mut [u8],
) -> Result<usize, ports::Error> {
    let ValueRef::Blob(bytes) = row.get_ref(column).map_err(sql)? else {
        return Err(ports::Error::Corrupt);
    };
    output
        .get_mut(..bytes.len())
        .ok_or(ports::Error::Capacity)?
        .copy_from_slice(bytes);
    Ok(bytes.len())
}
struct Budget {
    deadline: Deadline,
    last: ports::Tick,
    remaining: u64,
    failure: Option<ports::Error>,
    finishing_transaction: bool,
}
pub(in crate::store_fs) struct Native {
    connection: Mutex<Connection>,
    budget: Arc<Mutex<Budget>>,
    clock: Arc<dyn Clock>,
}
impl Native {
    fn open(path: &Path, clock: Arc<dyn Clock>, deadline: Deadline) -> Result<Self, ports::Error> {
        if rusqlite::version_number() != 3_053_002 {
            return Err(ports::Error::Invalid);
        }
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_FULL_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(sql)?;
        connection
            .busy_timeout(std::time::Duration::ZERO)
            .map_err(sql)?;
        let last = clock.sample()?.monotonic;
        if deadline.expired(last) {
            return Err(ports::Error::Deadline);
        }
        let budget = Arc::new(Mutex::new(Budget {
            deadline,
            last,
            remaining: VM_STEPS,
            failure: None,
            finishing_transaction: false,
        }));
        let hook = Arc::clone(&budget);
        let timer = Arc::clone(&clock);
        connection
            .progress_handler(
                1,
                Some(move || {
                    let Ok(mut budget) = hook.lock() else {
                        return true;
                    };
                    if budget.finishing_transaction {
                        return false;
                    }
                    if budget.failure.is_some() {
                        return true;
                    }
                    let outcome = timer.sample().and_then(|now| {
                        if now.monotonic < budget.last {
                            return Err(ports::Error::Invalid);
                        }
                        budget.last = now.monotonic;
                        if budget.deadline.expired(now.monotonic) {
                            return Err(ports::Error::Deadline);
                        }
                        budget.remaining = budget
                            .remaining
                            .checked_sub(1)
                            .ok_or(ports::Error::Capacity)?;
                        Ok(())
                    });
                    budget.failure = outcome.err();
                    budget.failure.is_some()
                }),
            )
            .map_err(sql)?;
        connection
            .execute_batch(concat!(
                "PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF; ",
                "PRAGMA temp_store=MEMORY; PRAGMA mmap_size=0; ",
                "PRAGMA cache_size=-128; PRAGMA cache_spill=ON; ",
                "PRAGMA synchronous=FULL; PRAGMA wal_autocheckpoint=0; ",
                "PRAGMA max_page_count=2097152;"
            ))
            .map_err(sql)?;
        let pages: i64 = connection
            .pragma_query_value(None, "max_page_count", |row| row.get(0))
            .map_err(sql)?;
        if u64::try_from(pages).ok() != Some(MAX_PAGES) {
            return Err(ports::Error::Invalid);
        }
        connection
            .set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
            .map_err(sql)?;
        connection
            .set_limit(
                rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH,
                crate::limits::SQLITE_VALUE_BYTES,
            )
            .map_err(sql)?;
        connection
            .set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_SQL_LENGTH, 8192)
            .map_err(sql)?;
        connection
            .set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_ATTACHED, 0)
            .map_err(sql)?;
        let heap: i64 = connection
            .query_row("PRAGMA hard_heap_limit", [], |row| row.get(0))
            .map_err(sql)?;
        if heap != 16_777_216 {
            return Err(ports::Error::Invalid);
        }
        Ok(Self {
            connection: Mutex::new(connection),
            budget,
            clock,
        })
    }
    fn begin_work(&self, deadline: Deadline) -> Result<(), ports::Error> {
        let now = self.clock.sample()?.monotonic;
        if deadline.expired(now) {
            return Err(ports::Error::Deadline);
        }
        *lock(&self.budget)? = Budget {
            deadline,
            last: now,
            remaining: VM_STEPS,
            failure: None,
            finishing_transaction: false,
        };
        Ok(())
    }
    fn checked_sample(&self) -> Result<ports::Time, ports::Error> {
        let mut budget = lock(&self.budget)?;
        if let Some(error) = budget.failure {
            return Err(error);
        }
        let result = self.clock.sample().and_then(|now| {
            if now.monotonic < budget.last {
                return Err(ports::Error::Invalid);
            }
            budget.last = now.monotonic;
            if budget.deadline.expired(now.monotonic) {
                return Err(ports::Error::Deadline);
            }
            Ok(now)
        });
        if let Err(error) = result {
            budget.failure = Some(error);
        }
        result
    }
    fn check(&self) -> Result<(), ports::Error> {
        self.checked_sample().map(|_| ())
    }
    fn run<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, ports::Error>,
    ) -> Result<T, ports::Error> {
        self.check()?;
        let connection = lock(&self.connection)?;
        let result = f(&connection);
        drop(connection);
        self.check()?;
        result
    }
    fn rollback(&self) -> bool {
        self.connection.lock().is_ok_and(|connection| {
            if connection.is_autocommit() {
                return true;
            }
            let Ok(mut budget) = self.budget.lock() else {
                return false;
            };
            // Cleanup must release the snapshot even after request fuel expires.
            budget.finishing_transaction = true;
            drop(budget);
            let result = connection.execute_batch("ROLLBACK").is_ok();
            let Ok(mut budget) = self.budget.lock() else {
                return false;
            };
            budget.finishing_transaction = false;
            result
        })
    }
}
impl Clock for Native {
    fn sample(&self) -> Result<ports::Time, ports::Error> {
        self.checked_sample()
    }
}
enum ReaderSlot {
    Available(Native),
    Borrowed,
    Retired,
}
struct Writer {
    native: Native,
    stopped: bool,
    scratch: Vec<u8>,
}
/// One database owner for a locked root. Constructor's exclusive borrow prevents
/// independent engines from bypassing its view and garbage-collection fence.
///
/// ```compile_fail,E0499
/// use std::sync::Arc;
/// use td_mta::{ports::{Clock, Deadline, Error}, store_fs::{IndexStore, LockedRoot}};
/// fn duplicate(root: &mut LockedRoot, clock: Arc<dyn Clock>, deadline: Deadline) -> Result<(), Error> {
///     let first = IndexStore::open(root, clock.clone(), 1, deadline)?;
///     let second = IndexStore::open(root, clock, 1, deadline)?;
///     let _ = (first.epoch(), second.epoch());
///     Ok(())
/// }
/// ```
pub struct IndexStore<'r> {
    root: &'r LockedRoot,
    epoch: StoreEpoch,
    writer: Mutex<Writer>,
    readers: Mutex<Vec<ReaderSlot>>,
    clock: Arc<dyn Clock>,
}
/// Passive request bounds; callers still authorize the account and admit work.
#[derive(Clone, Copy, Debug)]
pub struct CommitRequest {
    pub account: AccountId,
    pub expected: Sequence,
    pub utc_ms: i64,
    pub deadline: Deadline,
}
pub use crate::ports::CommitFailure as CommitError;
impl<'r> IndexStore<'r> {
    /// Cold startup only. The caller owns account authorization and resource
    /// admission. An old FORMAT store requires an explicit offline migration.
    pub fn create(
        root: &'r mut LockedRoot,
        epoch: StoreEpoch,
        clock: Arc<dyn Clock>,
        views: usize,
        deadline: Deadline,
    ) -> Result<Self, ports::Error> {
        if !(1..=8).contains(&views) {
            return Err(ports::Error::Invalid);
        }
        let path = db_path(root, RootEntry::Database)?;
        if optional_metadata(&db_path(root, RootEntry::Database)?.with_file_name("FORMAT"))?
            .is_some()
        {
            return Err(ports::Error::Invalid);
        }
        for name in [RootEntry::Wal, RootEntry::SharedMemory] {
            if optional_metadata(&db_path(root, name)?)?.is_some() {
                return Err(ports::Error::Invalid);
            }
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        initialize_database_file(&file)?;
        // Closing this fd later would release SQLite's process-wide POSIX locks.
        drop(file);
        root.root.directory.file.sync_all()?;
        let native = Native::open(&path, Arc::clone(&clock), deadline)?;
        native.run(|db| {
            db.execute_batch(
                "PRAGMA page_size=4096; PRAGMA journal_mode=WAL; PRAGMA max_page_count=2097152;",
            )
            .map_err(sql)?;
            db.execute_batch("BEGIN IMMEDIATE").map_err(sql)?;
            for statement in SCHEMA.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                db.execute_batch(statement).map_err(sql)?;
            }
            db.execute(
                "INSERT INTO store VALUES(1,?1)",
                [epoch.as_bytes().as_slice()],
            )
            .map_err(sql)?;
            db.pragma_update(None, "application_id", APP_ID)
                .map_err(sql)?;
            db.pragma_update(None, "user_version", 2).map_err(sql)?;
            db.execute_batch("COMMIT").map_err(sql)
        })?;
        root.root.directory.file.sync_all()?;
        Self::finish_open(root, native, clock, views, deadline)
    }
    pub fn open(
        root: &'r mut LockedRoot,
        clock: Arc<dyn Clock>,
        views: usize,
        deadline: Deadline,
    ) -> Result<Self, ports::Error> {
        if !(1..=8).contains(&views) {
            return Err(ports::Error::Invalid);
        }
        let path = db_path(root, RootEntry::Database)?;
        validate_file(root, &path, MAX_PAGES * PAGE_BYTES)?;
        for name in [RootEntry::Wal, RootEntry::SharedMemory] {
            let sidecar = db_path(root, name)?;
            if optional_metadata(&sidecar)?.is_some() {
                let maximum = if name == RootEntry::SharedMemory {
                    MAX_SHM_BYTES
                } else {
                    MAX_WAL_BYTES
                };
                validate_file(root, &sidecar, maximum)?;
            }
        }
        let native = Native::open(&path, Arc::clone(&clock), deadline)?;
        Self::finish_open(root, native, clock, views, deadline)
    }
    fn finish_open(
        root: &'r LockedRoot,
        native: Native,
        clock: Arc<dyn Clock>,
        views: usize,
        deadline: Deadline,
    ) -> Result<Self, ports::Error> {
        if !(1..=8).contains(&views) {
            return Err(ports::Error::Invalid);
        }
        let epoch = native.run(|db| {
            let app: i64 = db
                .pragma_query_value(None, "application_id", |row| row.get(0))
                .map_err(sql)?;
            let version: i64 = db
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(sql)?;
            let mode: String = db
                .pragma_query_value(None, "journal_mode", |row| row.get(0))
                .map_err(sql)?;
            let page_size: i64 = db
                .pragma_query_value(None, "page_size", |row| row.get(0))
                .map_err(sql)?;
            if app != APP_ID || version != 2 || mode != "wal" || page_size != PAGE_BYTES as i64 {
                return Err(ports::Error::Corrupt);
            }
            let expected = SCHEMA.split(';').map(str::trim).filter(|s| !s.is_empty());
            let count: i64 = db
                .query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*'",
                    [],
                    |row| row.get(0),
                )
                .map_err(sql)?;
            if count != expected.clone().count() as i64 {
                return Err(ports::Error::Corrupt);
            }
            for statement in expected {
                let name = statement
                    .split_whitespace()
                    .nth(if statement.starts_with("CREATE UNIQUE INDEX") {
                        3
                    } else {
                        2
                    })
                    .ok_or(ports::Error::Corrupt)?;
                let name = name.split('(').next().ok_or(ports::Error::Corrupt)?;
                let actual: String = db
                    .query_row(
                        "SELECT sql FROM sqlite_schema WHERE name=?1",
                        [name],
                        |row| row.get(0),
                    )
                    .map_err(sql)?;
                if actual != statement {
                    return Err(ports::Error::Corrupt);
                }
            }
            let bytes: [u8; 16] = db
                .query_row("SELECT epoch FROM store WHERE id=1", [], |row| row.get(0))
                .map_err(sql)?;
            Ok(StoreEpoch::from_bytes(bytes))
        })?;
        let mut readers = Vec::new();
        readers
            .try_reserve_exact(views)
            .map_err(|_| ports::Error::Capacity)?;
        let path = db_path(root, RootEntry::Database)?;
        for _ in 0..views {
            readers.push(ReaderSlot::Available(Native::open(
                &path,
                Arc::clone(&clock),
                deadline,
            )?));
        }
        let mut scratch = Vec::new();
        scratch
            .try_reserve_exact(2 * 65536)
            .map_err(|_| ports::Error::Capacity)?;
        scratch.resize(2 * 65536, 0);
        Ok(Self {
            root,
            epoch,
            writer: Mutex::new(Writer {
                native,
                stopped: false,
                scratch,
            }),
            readers: Mutex::new(readers),
            clock,
        })
    }
    fn writer(&self, deadline: Deadline) -> Result<MutexGuard<'_, Writer>, ports::Error> {
        if deadline.expired(self.clock.sample()?.monotonic) {
            return Err(ports::Error::Deadline);
        }
        self.writer.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => ports::Error::Busy,
            TryLockError::Poisoned(_) => ports::Error::WriterStopped,
        })
    }
    pub fn root(&self) -> &LockedRoot {
        self.root
    }
    pub fn epoch(&self) -> StoreEpoch {
        self.epoch
    }
    pub fn create_account(
        &self,
        account: AccountId,
        deadline: Deadline,
    ) -> Result<(), CommitError> {
        let mut writer = self.writer(deadline).map_err(CommitError::Rejected)?;
        if writer.stopped {
            return Err(CommitError::Rejected(ports::Error::WriterStopped));
        }
        writer
            .native
            .begin_work(deadline)
            .map_err(CommitError::Rejected)?;
        reserve_wal(self.root).map_err(CommitError::Rejected)?;
        let result = writer.native.run(|db| {
            let count: i64 = db
                .query_row("SELECT count(*) FROM accounts", [], |r| r.get(0))
                .map_err(sql)?;
            if count >= 128 {
                return Err(ports::Error::Capacity);
            }
            db.execute_batch("BEGIN IMMEDIATE").map_err(sql)?;
            db.execute(
                "INSERT INTO accounts VALUES(?1,?2,?2)",
                params![account.as_bytes().as_slice(), 0u64.to_be_bytes().as_slice()],
            )
            .map_err(sql)?;
            Ok(())
        });
        if let Err(error) = result {
            if !writer.native.rollback() {
                writer.stopped = true;
            }
            return Err(CommitError::Rejected(error));
        }
        finish_commit(&mut writer)
    }
    /// Capture the SQLite read transaction and account endpoint together while
    /// holding the writer fence. Drop ends it before returning the pool slot.
    pub fn view(
        &self,
        account: AccountId,
        deadline: Deadline,
    ) -> Result<IndexReadView<'_, 'r>, ports::Error> {
        let _writer = self.writer(deadline)?;
        let mut pool = lock(&self.readers)?;
        let slot = pool
            .iter()
            .position(|slot| matches!(slot, ReaderSlot::Available(_)))
            .ok_or(ports::Error::Busy)?;
        let entry = pool.get_mut(slot).ok_or(ports::Error::Corrupt)?;
        let ReaderSlot::Available(native) = std::mem::replace(entry, ReaderSlot::Borrowed) else {
            return Err(ports::Error::Corrupt);
        };
        drop(pool);
        let result = native.begin_work(deadline).and_then(|()| {
            native.run(|db| {
                db.execute_batch("BEGIN DEFERRED").map_err(sql)?;
                identity(db, account, self.epoch)
            })
        });
        match result {
            Ok(identity) => Ok(IndexReadView {
                store: self,
                slot,
                native: Some(native),
                identity,
                failed: None,
            }),
            Err(error) => {
                let returned = if native.rollback() {
                    ReaderSlot::Available(native)
                } else {
                    drop(native);
                    ReaderSlot::Retired
                };
                *lock(&self.readers)?
                    .get_mut(slot)
                    .ok_or(ports::Error::Corrupt)? = returned;
                Err(error)
            }
        }
    }
    /// Atomically store relational metadata and complete verified body streams.
    /// Every fresh blob requires exactly one matching source; no body survives rejection.
    pub fn commit<C: ports::Crypto>(
        &self,
        crypto: &C,
        request: CommitRequest,
        operations: &[Operation<'_>],
        sources: &mut [BlobSource<'_>],
    ) -> Result<Sequence, CommitError> {
        let mut writer = self
            .writer(request.deadline)
            .map_err(CommitError::Rejected)?;
        if writer.stopped {
            return Err(CommitError::Rejected(ports::Error::WriterStopped));
        }
        writer
            .native
            .begin_work(request.deadline)
            .map_err(CommitError::Rejected)?;
        let Writer {
            native, scratch, ..
        } = &mut *writer;
        let result = self.apply(native, crypto, request, operations, sources, scratch);
        match result {
            Ok(next) => {
                finish_commit(&mut writer)?;
                Ok(next)
            }
            Err(error) => {
                if !writer.native.rollback() {
                    writer.stopped = true;
                }
                Err(CommitError::Rejected(error))
            }
        }
    }
    fn apply<C: ports::Crypto>(
        &self,
        native: &Native,
        crypto: &C,
        request: CommitRequest,
        operations: &[Operation<'_>],
        sources: &mut [BlobSource<'_>],
        scratch: &mut [u8],
    ) -> Result<Sequence, ports::Error> {
        let (scratch, values) = scratch
            .split_at_mut_checked(65536)
            .ok_or(ports::Error::Corrupt)?;
        let CommitRequest {
            account,
            expected,
            utc_ms,
            ..
        } = request;
        if sources.len() > MAX_OPERATIONS
            || operations.is_empty()
            || operations.len() > MAX_OPERATIONS
        {
            return Err(ports::Error::Capacity);
        }
        for (ordinal, source) in sources.iter().enumerate() {
            if sources
                .get(..ordinal)
                .ok_or(ports::Error::Invalid)?
                .iter()
                .any(|other| other.id == source.id)
                || operations
                    .iter()
                    .filter(|operation| {
                        matches!(operation.value(),
                    Value::Row(Mutation::Put { key: Key::Blob(id), .. }) if id == source.id)
                    })
                    .count()
                    != 1
            {
                return Err(ports::Error::Invalid);
            }
        }
        let bytes = operations.iter().try_fold(0usize, |n, op| {
            n.checked_add(op.encoded_len().map_err(|_| ports::Error::Invalid)?)
                .ok_or(ports::Error::Capacity)
        })?;
        if bytes > MAX_TRANSACTION_BYTES {
            return Err(ports::Error::Capacity);
        }
        reserve_wal(self.root)?;
        native.run(|db| db.execute_batch("BEGIN IMMEDIATE").map_err(sql))?;
        let current = native.run(|db| identity(db, account, self.epoch))?;
        if current.committed_sequence != expected {
            return Err(ports::Error::Conflict);
        }
        let next = expected.successor().map_err(|_| ports::Error::Capacity)?;
        let identity = ViewIdentity {
            committed_sequence: next,
            ..current
        };
        let mut view = TransactionView { native, identity };
        for op in operations.iter().copied() {
            if let Value::Change(change) = op.value() {
                let key = change_key(change)?;
                let existed = view.get(key, scratch)?.is_some();
                if (change.action == ChangeAction::Created) == existed {
                    return Err(ports::Error::Invalid);
                }
            }
        }
        for (ordinal, op) in operations.iter().copied().enumerate() {
            native.check()?;
            match op.value() {
                Value::Row(Mutation::Put { key, row }) => {
                    let fresh = if let (Key::Blob(id), Row::Blob(blob)) = (key, row) {
                        if blob.length > MAX_BODY_BYTES {
                            return Err(ports::Error::Capacity);
                        }
                        let mut old = [0; 64];
                        match view.get(key, &mut old)? {
                            Some((Row::Blob(previous), _)) if previous == blob => {
                                if sources.iter().any(|source| source.id == id) {
                                    return Err(ports::Error::Invalid);
                                }
                                None
                            }
                            Some(_) => return Err(ports::Error::Conflict),
                            None => {
                                native.run(|db| {
                                    db.execute(
                                        "INSERT INTO blob_ids VALUES(?1,?2)",
                                        params![
                                            account.as_bytes().as_slice(),
                                            id.as_bytes().as_slice()
                                        ],
                                    )
                                    .map_err(sql)?;
                                    Ok(())
                                })?;
                                Some((id, blob))
                            }
                        }
                    } else {
                        None
                    };
                    native.run(|db| relational::put(db, account, key, row, next))?;
                    if let Some((id, blob)) = fresh {
                        let source = sources
                            .iter_mut()
                            .find(|source| source.id == id)
                            .ok_or(ports::Error::Invalid)?;
                        write_body(native, crypto, account, id, blob, source.source, scratch)?;
                    }
                }
                Value::Row(Mutation::Delete(key)) => {
                    native.run(|db| relational::delete(db, account, key))?;
                }
                Value::Change(change) => {
                    if change.kind == ObjectType::Identity {
                        return Err(ports::Error::Invalid);
                    }
                    let action = match change.action {
                        ChangeAction::Created => 1,
                        ChangeAction::Updated => 2,
                        ChangeAction::Destroyed => 3,
                    };
                    native.run(|db| {
                        db.execute(
                            "INSERT INTO changes VALUES(?1,?2,?3,?4,?5,?6)",
                            params![
                                account.as_bytes().as_slice(),
                                next.number().to_be_bytes().as_slice(),
                                ordinal as i64,
                                change.kind.tag(),
                                action,
                                change.id.as_slice()
                            ],
                        )
                        .map_err(sql)?;
                        Ok(())
                    })?;
                }
            }
        }
        for op in operations.iter().copied() {
            if let Value::Change(change) = op.value() {
                let exists = view.get(change_key(change)?, scratch)?.is_some();
                if (change.action == ChangeAction::Destroyed) == exists {
                    return Err(ports::Error::Invalid);
                }
            }
        }
        for op in operations.iter().copied() {
            let Value::Row(Mutation::Put { key, .. }) = op.value() else {
                continue;
            };
            let Some((row, changed)) = view.get(key, scratch)? else {
                continue;
            };
            let mut references = ReferenceCheck::new(identity, key, row, changed, utc_ms)
                .map_err(|_| ports::Error::Invalid)?;
            while !references
                .advance(&mut view, values)
                .map_err(|error| match error {
                    crate::row_references::Error::View(error) => error,
                    crate::row_references::Error::Format(_) => ports::Error::Corrupt,
                    _ => ports::Error::Conflict,
                })?
            {}
            if let Key::Mailbox(id) = key {
                let mut walk =
                    crate::mailbox_parents::ParentWalk::new(identity, id, MAX_OPERATIONS as u64)
                        .map_err(|_| ports::Error::Invalid)?;
                while !walk.is_complete() {
                    walk.advance(&mut view, values)
                        .map_err(|error| match error {
                            crate::mailbox_parents::Error::View(error) => error,
                            crate::mailbox_parents::Error::ReadLimit => ports::Error::Capacity,
                            crate::mailbox_parents::Error::Format(_) => ports::Error::Corrupt,
                            _ => ports::Error::Conflict,
                        })?;
                }
            }
        }
        native.run(|db| {
            db.execute(
                "UPDATE accounts SET sequence=?2 WHERE id=?1",
                params![
                    account.as_bytes().as_slice(),
                    next.number().to_be_bytes().as_slice()
                ],
            )
            .map_err(sql)?;
            Ok(())
        })?;
        Ok(next)
    }
    /// Full validation under the writer fence; new views and commits return Busy.
    pub fn validate_integrity(&self, deadline: Deadline) -> Result<(), ports::Error> {
        let writer = self.writer(deadline)?;
        if writer.stopped {
            return Err(ports::Error::WriterStopped);
        }
        writer.native.begin_work(deadline)?;
        lock(&writer.native.budget)?.remaining = INTEGRITY_VM_STEPS;
        writer.native.run(|db| {
            let check: String = db
                .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
                .map_err(sql)?;
            if check != "ok" {
                return Err(ports::Error::Corrupt);
            }
            let mut statement = db.prepare("PRAGMA foreign_key_check").map_err(sql)?;
            if statement
                .query([])
                .map_err(sql)?
                .next()
                .map_err(sql)?
                .is_some()
            {
                return Err(ports::Error::Corrupt);
            }
            Ok(())
        })
    }
    /// Reclaim WAL only when no view owns a SQLite read transaction.
    pub fn checkpoint(&self, deadline: Deadline) -> Result<(), ports::Error> {
        let writer = self.writer(deadline)?;
        if writer.stopped {
            return Err(ports::Error::WriterStopped);
        }
        if lock(&self.readers)?
            .iter()
            .any(|slot| matches!(slot, ReaderSlot::Borrowed))
        {
            return Err(ports::Error::Busy);
        }
        writer.native.begin_work(deadline)?;
        writer.native.run(|db| {
            let (busy, _, _): (i64, i64, i64) = db
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .map_err(sql)?;
            if busy != 0 {
                return Err(ports::Error::Busy);
            }
            Ok(())
        })?;
        self.root.root.directory.file.sync_all()?;
        Ok(())
    }
}
fn body_rowid(db: &Connection, account: AccountId, id: BlobId) -> Result<i64, ports::Error> {
    db.query_row(
        "SELECT rowid FROM blobs WHERE account=?1 AND id=?2",
        params![account.as_bytes().as_slice(), id.as_bytes().as_slice()],
        |row| row.get(0),
    )
    .map_err(sql)
}
fn write_body<C: ports::Crypto>(
    native: &Native,
    crypto: &C,
    account: AccountId,
    id: BlobId,
    expected: format::row::BlobRow,
    source: &mut dyn std::io::Read,
    scratch: &mut [u8],
) -> Result<(), ports::Error> {
    let mut digest = crypto.sha256()?;
    native.run(|db| {
        let mut insert = db
            .prepare("INSERT INTO blob_chunks(account,blob,ordinal,body) VALUES(?1,?2,?3,?4)")
            .map_err(sql)?;
        let mut position = 0_u64;
        let mut ordinal = 0_i64;
        while position < expected.length {
            let count = usize::try_from(expected.length - position)
                .map_err(|_| ports::Error::Capacity)?
                .min(super::MAX_FILE_STEP_BYTES);
            let bytes = scratch.get_mut(..count).ok_or(ports::Error::Capacity)?;
            let mut filled = 0;
            while filled < count {
                native.check()?;
                let read = source.read(bytes.get_mut(filled..).ok_or(ports::Error::Corrupt)?)?;
                native.check()?;
                if read == 0 || read > count - filled {
                    return Err(ports::Error::Corrupt);
                }
                filled += read;
            }
            digest.update(bytes)?;
            insert
                .execute(params![
                    account.as_bytes().as_slice(),
                    id.as_bytes().as_slice(),
                    ordinal,
                    &*bytes
                ])
                .map_err(sql)?;
            position += count as u64;
            ordinal += 1;
            native.check()?;
        }
        let mut extra = [0; 1];
        native.check()?;
        let count = source.read(&mut extra)?;
        native.check()?;
        if count != 0 || !crypto.equal_digest(&digest.finish()?, &expected.digest) {
            return Err(ports::Error::Corrupt);
        }
        Ok(())
    })
}
impl Native {
    pub(in crate::store_fs) fn read_body(
        &self,
        rowid: i64,
        length: u64,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize, ports::Error> {
        let remaining = length.checked_sub(offset).ok_or(ports::Error::Invalid)?;
        let count = usize::try_from(remaining)
            .map_err(|_| ports::Error::Capacity)?
            .min(output.len())
            .min(super::MAX_FILE_STEP_BYTES);
        self.run(|db| {
            let mut query = db.prepare("SELECT c.body FROM blobs AS b JOIN blob_chunks AS c ON c.account=b.account AND c.blob=b.id WHERE b.rowid=?1 AND c.ordinal=?2").map_err(sql)?;
            let mut copied = 0;
            while copied < count {
                let position = offset + copied as u64;
                let chunk_bytes = super::MAX_FILE_STEP_BYTES as u64;
                let ordinal = position / chunk_bytes;
                let within = (position % chunk_bytes) as usize;
                let expected = (length - ordinal * chunk_bytes).min(chunk_bytes) as usize;
                let mut rows = query.query(params![rowid, i64::try_from(ordinal).map_err(|_| ports::Error::Invalid)?]).map_err(sql)?;
                let row = rows.next().map_err(sql)?.ok_or(ports::Error::Corrupt)?;
                let ValueRef::Blob(bytes) = row.get_ref(0).map_err(sql)? else {
                    return Err(ports::Error::Corrupt);
                };
                if bytes.len() != expected {
                    return Err(ports::Error::Corrupt);
                }
                let take = (expected - within).min(count - copied);
                output.get_mut(copied..copied + take).ok_or(ports::Error::Corrupt)?
                    .copy_from_slice(bytes.get(within..within + take).ok_or(ports::Error::Corrupt)?);
                copied += take;
            }
            Ok(count)
        })
    }
}

fn db_path(root: &LockedRoot, entry: RootEntry) -> Result<std::path::PathBuf, ports::Error> {
    let path = std::str::from_utf8(
        root.root
            .directory
            .path
            .get(..root.root.directory.length)
            .ok_or(ports::Error::Invalid)?,
    )
    .map_err(|_| ports::Error::Invalid)?;
    let name = Name::root(entry).map_err(|_| ports::Error::Invalid)?;
    let full = Path::new(path).join(name.as_path().map_err(|_| ports::Error::Invalid)?);
    if full.as_os_str().len() > super::MAX_PATH_BYTES {
        return Err(ports::Error::Capacity);
    }
    Ok(full)
}
fn validate_file(root: &LockedRoot, path: &Path, maximum: u64) -> Result<(), ports::Error> {
    let metadata = fs::symlink_metadata(path)?;
    let owner = root.root.directory.metadata()?.uid();
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.nlink() != 1
        || metadata.mode() & 0o7777 != 0o600
        || metadata.len() > maximum
    {
        return Err(ports::Error::Invalid);
    }
    Ok(())
}
fn initialize_database_file(file: &fs::File) -> Result<(), ports::Error> {
    // OpenOptions creation mode is filtered by umask; restore only owner bits.
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.sync_all()?;
    Ok(())
}
fn finish_commit(writer: &mut Writer) -> Result<(), CommitError> {
    if let Err(error) = writer.native.check() {
        if !writer.native.rollback() {
            writer.stopped = true;
        }
        return Err(CommitError::Rejected(error));
    }
    let (result, autocommit, rejected) = {
        let connection = match lock(&writer.native.connection) {
            Ok(connection) => connection,
            Err(error) => {
                writer.stopped = true;
                return Err(CommitError::Rejected(error));
            }
        };
        let mut budget = match lock(&writer.native.budget) {
            Ok(budget) => budget,
            Err(error) => {
                writer.stopped = true;
                return Err(CommitError::Rejected(error));
            }
        };
        // SQLite can call progress after fsync and mask the durable outcome.
        // Finish the primitive without interruption, preserving sticky failure.
        budget.finishing_transaction = true;
        drop(budget);
        let result = connection.execute_batch("COMMIT");
        let autocommit = connection.is_autocommit();
        match lock(&writer.native.budget) {
            Ok(mut budget) => budget.finishing_transaction = false,
            Err(_) => writer.stopped = true,
        }
        let rejected = !autocommit
            && result.as_ref().is_err_and(|error| match error {
                rusqlite::Error::SqliteFailure(error, _) => matches!(
                    error.code,
                    rusqlite::ErrorCode::ConstraintViolation
                        | rusqlite::ErrorCode::DatabaseBusy
                        | rusqlite::ErrorCode::DatabaseLocked
                ),
                _ => false,
            });
        (result, autocommit, rejected)
    };
    // Sampling after completion records lateness without changing a known result.
    let _ = writer.native.check();
    match result {
        Ok(()) if autocommit => {
            // A late deadline cannot undo or obscure a successful durable commit.
            Ok(())
        }
        Err(error) if rejected => {
            let error = sql(error);
            if !writer.native.rollback() {
                writer.stopped = true;
            }
            Err(CommitError::Rejected(error))
        }
        result => {
            let error = match writer.native.check() {
                Err(error) => error,
                Ok(()) => match result {
                    Err(error) => sql(error),
                    Ok(()) => ports::Error::Corrupt,
                },
            };
            let _ = writer.native.rollback();
            writer.stopped = true;
            Err(CommitError::Indeterminate(error))
        }
    }
}

fn identity(
    db: &Connection,
    account: AccountId,
    epoch: StoreEpoch,
) -> Result<ViewIdentity, ports::Error> {
    let mut statement = db
        .prepare("SELECT sequence,floor FROM accounts WHERE id=?1")
        .map_err(sql)?;
    let mut rows = statement
        .query([account.as_bytes().as_slice()])
        .map_err(sql)?;
    let row = rows.next().map_err(sql)?.ok_or(ports::Error::NotFound)?;
    let mut endpoint = [0; 8];
    let mut floor = [0; 8];
    copy_blob(row, 0, &mut endpoint)?;
    copy_blob(row, 1, &mut floor)?;
    Ok(ViewIdentity {
        account,
        epoch,
        committed_sequence: sequence(&endpoint)?,
        history_floor: sequence(&floor)?,
    })
}
struct TransactionView<'a> {
    native: &'a Native,
    identity: ViewIdentity,
}
impl ReadView for TransactionView<'_> {
    fn identity(&self) -> ViewIdentity {
        self.identity
    }
    fn get<'a>(
        &mut self,
        key: Key<'_>,
        value: &'a mut [u8],
    ) -> Result<Option<(Row<'a>, Sequence)>, ports::Error> {
        get(self.native, self.identity, key, value)
    }
    fn next<'a>(
        &mut self,
        table: Table,
        after: Option<&[u8]>,
        key: &'a mut [u8],
        value: &'a mut [u8],
    ) -> Result<Option<Record<'a>>, ports::Error> {
        next(self.native, self.identity, table, after, key, value)
    }
    fn next_change(
        &mut self,
        after: ChangeCursor,
        kind: ObjectType,
    ) -> Result<ChangeStep, ports::Error> {
        next_change(self.native, self.identity, after, kind)
    }
}
/// A bounded pool lease holding a real SQLite WAL snapshot. It is neither Copy
/// nor Clone; a body borrowed from it keeps this lease alive.
/// Body custody prevents returning its database snapshot to the pool.
///
/// ```compile_fail,E0505
/// use td_mta::{ids::BlobId, ports::{BlobReader, Error}, store_fs::IndexReadView};
/// fn release(mut view: IndexReadView<'_, '_>) -> Result<(), Error> {
///     let body = view.open_blob_input(&td_crypto::Provider, BlobId::from_bytes([1; 16]), 1024)?.finish()?;
///     drop(view);
///     let _ = body.len();
///     Ok(())
/// }
/// ```
pub struct IndexReadView<'s, 'r> {
    store: &'s IndexStore<'r>,
    slot: usize,
    native: Option<Native>,
    identity: ViewIdentity,
    failed: Option<ports::Error>,
}
impl IndexReadView<'_, '_> {
    fn native(&self) -> Result<&Native, ports::Error> {
        self.native.as_ref().ok_or(ports::Error::WriterStopped)
    }
    fn read_snapshot<T>(
        &mut self,
        read: impl FnOnce(&Native) -> Result<T, ports::Error>,
    ) -> Result<T, ports::Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let native = self.native()?;
        // SQLite may end a read transaction after NOMEM, IOERR or FULL.
        let live = lock(&native.connection).map(|db| !db.is_autocommit());
        if !matches!(live, Ok(true)) {
            let error = live.err().unwrap_or(ports::Error::Corrupt);
            self.failed = Some(error);
            return Err(error);
        }
        let result = read(native);
        let live = lock(&native.connection).map(|db| !db.is_autocommit());
        if !matches!(live, Ok(true)) {
            let error = live
                .err()
                .or_else(|| result.as_ref().err().copied())
                .unwrap_or(ports::Error::Corrupt);
            self.failed = Some(error);
            return Err(error);
        }
        result
    }
    pub(in crate::store_fs) fn blob_scope(&mut self) -> Result<(&Native, Deadline), ports::Error> {
        let deadline = self.read_snapshot(|native| {
            native.check()?;
            Ok(lock(&native.budget)?.deadline)
        })?;
        Ok((self.native()?, deadline))
    }
    pub(in crate::store_fs) fn blob_rowid(&mut self, id: BlobId) -> Result<i64, ports::Error> {
        let account = self.identity.account;
        self.read_snapshot(|native| native.run(|db| body_rowid(db, account, id)))
    }
    pub fn root(&self) -> &LockedRoot {
        self.store.root
    }
}
impl ReadView for IndexReadView<'_, '_> {
    fn identity(&self) -> ViewIdentity {
        self.identity
    }
    fn get<'a>(
        &mut self,
        key: Key<'_>,
        value: &'a mut [u8],
    ) -> Result<Option<(Row<'a>, Sequence)>, ports::Error> {
        let identity = self.identity;
        self.read_snapshot(|native| get(native, identity, key, value))
    }
    fn next<'a>(
        &mut self,
        table: Table,
        after: Option<&[u8]>,
        key: &'a mut [u8],
        value: &'a mut [u8],
    ) -> Result<Option<Record<'a>>, ports::Error> {
        let identity = self.identity;
        self.read_snapshot(|native| next(native, identity, table, after, key, value))
    }
    fn next_change(
        &mut self,
        after: ChangeCursor,
        kind: ObjectType,
    ) -> Result<ChangeStep, ports::Error> {
        let identity = self.identity;
        self.read_snapshot(|native| next_change(native, identity, after, kind))
    }
}
impl Drop for IndexReadView<'_, '_> {
    fn drop(&mut self) {
        if let Some(native) = self.native.take() {
            let returned = if self.failed.is_some() {
                drop(native);
                ReaderSlot::Retired
            } else if native.rollback() {
                ReaderSlot::Available(native)
            } else {
                drop(native);
                ReaderSlot::Retired
            };
            if let Ok(mut pool) = self.store.readers.lock() {
                if let Some(slot) = pool.get_mut(self.slot) {
                    *slot = returned;
                }
            }
        }
    }
}
fn get<'a>(
    native: &Native,
    identity: ViewIdentity,
    key: Key<'_>,
    value: &'a mut [u8],
) -> Result<Option<(Row<'a>, Sequence)>, ports::Error> {
    key.validate_local().map_err(|_| ports::Error::Invalid)?;
    let result = native.run(|db| relational::get(db, identity.account, key, value))?;
    let Some((length, changed)) = result else {
        return Ok(None);
    };
    let row = Row::decode(
        key.table(),
        value.get(..length).ok_or(ports::Error::Corrupt)?,
    )
    .map_err(|_| ports::Error::Corrupt)?;
    row.validate_key(key).map_err(|_| ports::Error::Corrupt)?;
    if changed > identity.committed_sequence {
        return Err(ports::Error::Corrupt);
    }
    Ok(Some((row, changed)))
}
fn next<'a>(
    native: &Native,
    identity: ViewIdentity,
    table: Table,
    after: Option<&[u8]>,
    key: &'a mut [u8],
    value: &'a mut [u8],
) -> Result<Option<Record<'a>>, ports::Error> {
    if let Some(after) = after {
        Key::decode(table, after)
            .and_then(Key::validate_local)
            .map_err(|_| ports::Error::Invalid)?;
    }
    let found =
        native.run(|db| relational::next(db, identity.account, table, after, key, value))?;
    let Some((kl, vl, last_change)) = found else {
        return Ok(None);
    };
    let (key, row) = format::row::decode_record(
        table,
        key.get(..kl).ok_or(ports::Error::Corrupt)?,
        value.get(..vl).ok_or(ports::Error::Corrupt)?,
    )
    .map_err(|_| ports::Error::Corrupt)?;
    if last_change > identity.committed_sequence {
        return Err(ports::Error::Corrupt);
    }
    Ok(Some(Record {
        key,
        row,
        last_change,
    }))
}
fn next_change(
    native: &Native,
    identity: ViewIdentity,
    after: ChangeCursor,
    kind: ObjectType,
) -> Result<ChangeStep, ports::Error> {
    if after.sequence < identity.history_floor {
        return Err(ports::Error::HistoryLost);
    }
    if after.sequence > identity.committed_sequence {
        return Err(ports::Error::Invalid);
    }
    native.run(|db| {
        let mut statement = db.prepare(NEXT_CHANGE).map_err(sql)?;
        let mut rows = statement
            .query(params![
                identity.account.as_bytes().as_slice(),
                kind.tag(),
                after.sequence.number().to_be_bytes().as_slice(),
                i64::from(after.operation),
                identity
                    .committed_sequence
                    .number()
                    .to_be_bytes()
                    .as_slice()
            ])
            .map_err(sql)?;
        let Some(row) = rows.next().map_err(sql)? else {
            return Ok(ChangeStep::Complete);
        };
        let mut seq = [0; 8];
        copy_blob(row, 0, &mut seq)?;
        let operation: u32 = row.get(1).map_err(sql)?;
        let action: i64 = row.get(2).map_err(sql)?;
        let action = match action {
            1 => ChangeAction::Created,
            2 => ChangeAction::Updated,
            3 => ChangeAction::Destroyed,
            _ => return Err(ports::Error::Corrupt),
        };
        let mut id = [0; 16];
        copy_blob(row, 3, &mut id)?;
        Ok(ChangeStep::Record(ChangeRecord {
            cursor: ChangeCursor {
                sequence: sequence(&seq)?,
                operation,
            },
            change: Change { kind, id, action },
        }))
    })
}

fn optional_metadata(path: &Path) -> Result<Option<fs::Metadata>, ports::Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
fn reserve_wal(root: &LockedRoot) -> Result<(), ports::Error> {
    let path = db_path(root, RootEntry::Wal)?;
    if let Some(metadata) = optional_metadata(&path)? {
        validate_file(root, &path, MAX_WAL_BYTES)?;
        if metadata.len() > MAX_WAL_BYTES - TRANSACTION_WAL_BYTES {
            return Err(ports::Error::Busy);
        }
    }
    Ok(())
}

fn change_key(change: Change) -> Result<Key<'static>, ports::Error> {
    use crate::ids::{EmailId, MailboxId, SubmissionId, ThreadId};
    match change.kind {
        ObjectType::Mailbox => Ok(Key::Mailbox(MailboxId::from_bytes(change.id))),
        ObjectType::Thread => Ok(Key::Thread(ThreadId::from_bytes(change.id))),
        ObjectType::Email => Ok(Key::Email(EmailId::from_bytes(change.id))),
        ObjectType::EmailSubmission => Ok(Key::Submission(SubmissionId::from_bytes(change.id))),
        ObjectType::Identity => Err(ports::Error::Invalid),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::super::tests::Fixture;
    use super::*;
    use crate::{
        format::row::{BlobKind, BlobRow, EmailOrigin, EmailRow, LeaseRow, LeaseUse, MailboxRow},
        ids::{BlobId, DeviceId, EmailId, MailboxId, ThreadId},
        ports::{Crypto, Digest, Tick, Time},
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Timer(AtomicU64);
    impl Clock for Timer {
        fn sample(&self) -> Result<Time, ports::Error> {
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(self.0.load(Ordering::Relaxed)),
            })
        }
    }
    fn deadline() -> Deadline {
        Deadline::after(Tick(0), 100).unwrap()
    }
    fn encode(row: Row<'_>) -> Vec<u8> {
        let mut bytes = vec![0; 65536];
        let n = row.encode(&mut bytes).unwrap();
        bytes.truncate(n);
        bytes
    }
    fn mailbox(name: &str, parent: Option<MailboxId>) -> Vec<u8> {
        encode(Row::Mailbox(MailboxRow {
            name,
            parent,
            role: None,
            sort_order: 0,
            subscribed: true,
        }))
    }
    const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
    const ID: MailboxId = MailboxId::from_bytes([2; 16]);
    const MEMBERSHIP_EMAIL: &str = "SELECT email_id FROM memberships INDEXED BY memberships_mailbox WHERE account=?1 AND mailbox_id=?2";
    #[test]
    fn lost_read_transactions_never_resume_under_an_old_view_identity() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let clock = Arc::new(Timer(AtomicU64::new(1)));
        let epoch = StoreEpoch::from_bytes([9; 16]);
        let store = IndexStore::create(&mut root, epoch, clock.clone(), 2, deadline()).unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let old = mailbox("old", None);
        let operation = Operation::put(Table::Mailboxes, ID.as_bytes(), &old).unwrap();
        store
            .commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    expected: Sequence::default(),
                    utc_ms: 0,
                    deadline: deadline(),
                },
                &[operation],
                &mut [],
            )
            .unwrap();
        let mut failed = store.view(ACCOUNT, deadline()).unwrap();
        let mut vanished = store.view(ACCOUNT, deadline()).unwrap();
        let interrupted: Result<(), ports::Error> = failed.read_snapshot(|native| {
            native.run(|db| {
                db.execute_batch("ROLLBACK").map_err(sql)?;
                Err(ports::Error::Capacity)
            })
        });
        assert_eq!(interrupted, Err(ports::Error::Capacity));
        vanished
            .native()
            .unwrap()
            .run(|db| db.execute_batch("ROLLBACK").map_err(sql))
            .unwrap();
        let updated = mailbox("new", None);
        let operation = Operation::put(Table::Mailboxes, ID.as_bytes(), &updated).unwrap();
        store
            .commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    expected: Sequence::from_u64(1),
                    utc_ms: 0,
                    deadline: deadline(),
                },
                &[operation],
                &mut [],
            )
            .unwrap();
        assert_eq!(failed.identity().committed_sequence, Sequence::from_u64(1));
        assert_eq!(
            vanished.identity().committed_sequence,
            Sequence::from_u64(1)
        );
        assert_eq!(
            failed.get(Key::Mailbox(ID), &mut [0; 128]),
            Err(ports::Error::Capacity)
        );
        assert_eq!(
            vanished.get(Key::Mailbox(ID), &mut [0; 128]),
            Err(ports::Error::Corrupt)
        );
        fn assert_lost_view(view: &mut IndexReadView<'_, '_>, expected: ports::Error) {
            view.native()
                .unwrap()
                .run(|db| db.execute_batch("BEGIN DEFERRED").map_err(sql))
                .unwrap();
            assert_eq!(
                view.next(Table::Mailboxes, None, &mut [0; 64], &mut [0; 128]),
                Err(expected)
            );
            assert_eq!(
                view.next_change(
                    ChangeCursor {
                        sequence: Sequence::default(),
                        operation: u32::MAX,
                    },
                    ObjectType::Mailbox,
                ),
                Err(expected)
            );
            assert!(matches!(
                view.open_blob_input(&td_crypto::Provider, BlobId::from_bytes([3; 16]), 32),
                Err(error) if error == expected
            ));
        }
        assert_lost_view(&mut failed, ports::Error::Capacity);
        assert_lost_view(&mut vanished, ports::Error::Corrupt);
        drop(failed);
        drop(vanished);
        assert!(matches!(
            store.view(ACCOUNT, deadline()),
            Err(ports::Error::Busy)
        ));
        drop(store);
        let reopened = IndexStore::open(&mut root, clock, 1, deadline()).unwrap();
        let mut fresh = reopened.view(ACCOUNT, deadline()).unwrap();
        assert!(matches!(
            fresh.get(Key::Mailbox(ID), &mut [0; 128]).unwrap(),
            Some((Row::Mailbox(MailboxRow { name: "new", .. }), _))
        ));
    }
    #[test]
    fn sqlite_transactions_reopen_and_snapshot_real_metadata() {
        let fixture = Fixture::maximum_root();
        let mut root = fixture.locked();
        let timer = Arc::new(Timer(AtomicU64::new(1)));
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            timer.clone(),
            2,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let initial = mailbox("one", None);
        let op = Operation::put(Table::Mailboxes, ID.as_bytes(), &initial).unwrap();
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    expected: Sequence::default(),
                    utc_ms: 0,
                    deadline: deadline()
                },
                &[op],
                &mut []
            ),
            Ok(Sequence::from_u64(1))
        );
        let mut first = store.view(ACCOUNT, deadline()).unwrap();
        let changed = mailbox("two", None);
        let op = Operation::put(Table::Mailboxes, ID.as_bytes(), &changed).unwrap();
        store
            .commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    expected: Sequence::from_u64(1),
                    utc_ms: 0,
                    deadline: deadline(),
                },
                &[op],
                &mut [],
            )
            .unwrap();
        let mut second = store.view(ACCOUNT, deadline()).unwrap();
        let mut bytes = [0; 128];
        assert!(matches!(
            first.get(Key::Mailbox(ID), &mut bytes).unwrap(),
            Some((Row::Mailbox(MailboxRow { name: "one", .. }), _))
        ));
        assert!(matches!(
            second.get(Key::Mailbox(ID), &mut bytes).unwrap(),
            Some((Row::Mailbox(MailboxRow { name: "two", .. }), _))
        ));
        assert!(matches!(
            store.view(ACCOUNT, deadline()),
            Err(ports::Error::Busy)
        ));
        assert_eq!(store.checkpoint(deadline()), Err(ports::Error::Busy));
        drop(first);
        drop(second);
        store.checkpoint(deadline()).unwrap();
        drop(store);
        let reopened = IndexStore::open(&mut root, timer, 1, deadline()).unwrap();
        let mut view = reopened.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(view.identity().committed_sequence, Sequence::from_u64(2));
        assert!(matches!(
            view.get(Key::Mailbox(ID), &mut bytes).unwrap(),
            Some((Row::Mailbox(MailboxRow { name: "two", .. }), _))
        ));
        assert_eq!(
            &fs::read(fixture.path.join("metadata.sqlite3")).unwrap()[..16],
            b"SQLite format 3\0"
        );
    }
    #[test]
    fn failed_batches_roll_back_and_references_and_cycles_are_enforced() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            Arc::new(Timer(AtomicU64::new(1))),
            2,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let parent = MailboxId::from_bytes([3; 16]);
        let orphan = mailbox("child", Some(parent));
        let child = Operation::put(Table::Mailboxes, ID.as_bytes(), &orphan).unwrap();
        assert!(matches!(
            store.commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    expected: Sequence::default(),
                    utc_ms: 0,
                    deadline: deadline()
                },
                &[child],
                &mut []
            ),
            Err(CommitError::Rejected(ports::Error::Conflict))
        ));
        let parent_bytes = mailbox("parent", None);
        let parent_op = Operation::put(Table::Mailboxes, parent.as_bytes(), &parent_bytes).unwrap();
        store
            .commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    expected: Sequence::default(),
                    utc_ms: 0,
                    deadline: deadline(),
                },
                &[child, parent_op],
                &mut [],
            )
            .unwrap();
        let delete = Operation::delete(Table::Mailboxes, parent.as_bytes()).unwrap();
        assert!(matches!(
            store.commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    expected: Sequence::from_u64(1),
                    utc_ms: 0,
                    deadline: deadline()
                },
                &[delete],
                &mut []
            ),
            Err(CommitError::Rejected(ports::Error::Conflict))
        ));
        let cycle = mailbox("parent", Some(ID));
        let op = Operation::put(Table::Mailboxes, parent.as_bytes(), &cycle).unwrap();
        assert!(matches!(
            store.commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    expected: Sequence::from_u64(1),
                    utc_ms: 0,
                    deadline: deadline()
                },
                &[op],
                &mut []
            ),
            Err(CommitError::Rejected(ports::Error::Conflict))
        ));
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        let mut bytes = [0; 128];
        assert!(view
            .get(Key::Mailbox(parent), &mut bytes)
            .unwrap()
            .is_some());
        assert_eq!(view.identity().committed_sequence, Sequence::from_u64(1));
    }
    #[test]
    fn native_deadline_is_sticky_and_unsigned_sequences_are_exact() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let timer = Arc::new(Timer(AtomicU64::new(1)));
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            timer.clone(),
            1,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        timer.0.store(100, Ordering::Relaxed);
        let mut bytes = [0; 64];
        assert_eq!(
            view.get(Key::Mailbox(ID), &mut bytes),
            Err(ports::Error::Deadline)
        );
        timer.0.store(1, Ordering::Relaxed);
        assert_eq!(
            view.get(Key::Mailbox(ID), &mut bytes),
            Err(ports::Error::Deadline)
        );
        drop(view);
        lock(&store.writer)
            .unwrap()
            .native
            .run(|db| {
                db.execute(
                    "UPDATE accounts SET sequence=?2 WHERE id=?1",
                    params![
                        ACCOUNT.as_bytes().as_slice(),
                        (u64::MAX - 1).to_be_bytes().as_slice()
                    ],
                )
                .map_err(sql)?;
                Ok(())
            })
            .unwrap();
        let row = mailbox("last", None);
        let op = Operation::put(Table::Mailboxes, ID.as_bytes(), &row).unwrap();
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    expected: Sequence::from_u64(u64::MAX - 1),
                    utc_ms: 0,
                    deadline: deadline()
                },
                &[op],
                &mut []
            ),
            Ok(Sequence::from_u64(u64::MAX))
        );
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    expected: Sequence::from_u64(u64::MAX),
                    utc_ms: 0,
                    deadline: deadline()
                },
                &[op],
                &mut []
            ),
            Err(CommitError::Rejected(ports::Error::Capacity))
        );
    }
    #[test]
    fn changes_use_native_order_and_reject_incoherent_or_duplicate_actions() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            Arc::new(Timer(AtomicU64::new(1))),
            2,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let value = mailbox("one", None);
        let put = Operation::put(Table::Mailboxes, ID.as_bytes(), &value).unwrap();
        let created = Operation::change(ObjectType::Mailbox, ChangeAction::Created, ID.as_bytes());
        let request = CommitRequest {
            account: ACCOUNT,
            expected: Sequence::default(),
            utc_ms: 0,
            deadline: deadline(),
        };
        assert_eq!(
            store.commit(&td_crypto::Provider, request, &[put, created], &mut []),
            Ok(Sequence::from_u64(1))
        );
        let mut old = store.view(ACCOUNT, deadline()).unwrap();
        let request = CommitRequest {
            expected: Sequence::from_u64(1),
            ..request
        };
        assert_eq!(
            store.commit(&td_crypto::Provider, request, &[created], &mut []),
            Err(CommitError::Rejected(ports::Error::Invalid))
        );
        let updated = Operation::change(ObjectType::Mailbox, ChangeAction::Updated, ID.as_bytes());
        assert_eq!(
            store.commit(&td_crypto::Provider, request, &[updated, updated], &mut []),
            Err(CommitError::Rejected(ports::Error::Conflict))
        );
        let deleted = Operation::delete(Table::Mailboxes, ID.as_bytes()).unwrap();
        let destroyed =
            Operation::change(ObjectType::Mailbox, ChangeAction::Destroyed, ID.as_bytes());
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                request,
                &[deleted, destroyed],
                &mut []
            ),
            Ok(Sequence::from_u64(2))
        );
        let start = ChangeCursor {
            sequence: Sequence::default(),
            operation: u32::MAX,
        };
        let ChangeStep::Record(first) = old.next_change(start, ObjectType::Mailbox).unwrap() else {
            panic!("missing created change")
        };
        assert_eq!(first.change.action, ChangeAction::Created);
        assert_eq!(first.cursor.sequence, Sequence::from_u64(1));
        assert_eq!(first.cursor.operation, 1);
        assert_eq!(
            old.next_change(first.cursor, ObjectType::Mailbox).unwrap(),
            ChangeStep::Complete
        );
        let mut current = store.view(ACCOUNT, deadline()).unwrap();
        let ChangeStep::Record(last) = current
            .next_change(first.cursor, ObjectType::Mailbox)
            .unwrap()
        else {
            panic!("missing destroyed change")
        };
        assert_eq!(last.change.action, ChangeAction::Destroyed);
        assert_eq!(last.cursor.sequence, Sequence::from_u64(2));
        assert_eq!(
            current
                .next_change(last.cursor, ObjectType::Mailbox)
                .unwrap(),
            ChangeStep::Complete
        );
    }
    #[test]
    fn startup_refuses_unknown_schema_and_dangling_sidecars() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let timer = Arc::new(Timer(AtomicU64::new(1)));
        assert!(matches!(
            IndexStore::create(
                &mut root,
                StoreEpoch::from_bytes([9; 16]),
                timer.clone(),
                0,
                deadline()
            ),
            Err(ports::Error::Invalid)
        ));
        assert!(!fixture.path.join("metadata.sqlite3").exists());
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            timer.clone(),
            1,
            deadline(),
        )
        .unwrap();
        store
            .writer
            .lock()
            .unwrap()
            .native
            .run(|db| {
                db.execute_batch("CREATE VIEW unknown_view AS SELECT id FROM accounts")
                    .map_err(sql)
            })
            .unwrap();
        drop(store);
        assert!(matches!(
            IndexStore::open(&mut root, timer.clone(), 1, deadline()),
            Err(ports::Error::Corrupt)
        ));
        let path = fixture.path.join("metadata.sqlite3-wal");
        if optional_metadata(&path).unwrap().is_some() {
            fs::remove_file(&path).unwrap();
        }
        std::os::unix::fs::symlink(fixture.path.join("missing"), &path).unwrap();
        assert!(matches!(
            IndexStore::open(&mut root, timer, 1, deadline()),
            Err(ports::Error::Invalid)
        ));
    }
    #[test]
    fn short_native_queries_consume_the_original_vm_fuel() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            Arc::new(Timer(AtomicU64::new(1))),
            1,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        lock(&view.native().unwrap().budget).unwrap().remaining = 1;
        let mut bytes = [0; 128];
        assert!(matches!(
            view.get(Key::Mailbox(ID), &mut bytes),
            Err(ports::Error::Capacity)
        ));
        assert!(matches!(
            view.get(Key::Mailbox(ID), &mut bytes),
            Err(ports::Error::Capacity)
        ));
    }
    #[test]
    fn successful_commit_reports_durability_even_when_its_deadline_expires() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let timer = Arc::new(Timer(AtomicU64::new(1)));
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            timer.clone(),
            1,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let hook = timer.clone();
        store
            .writer
            .lock()
            .unwrap()
            .native
            .connection
            .lock()
            .unwrap()
            .commit_hook(Some(move || {
                hook.0.store(100, Ordering::Relaxed);
                false
            }))
            .unwrap();
        let value = mailbox("committed", None);
        let op = Operation::put(Table::Mailboxes, ID.as_bytes(), &value).unwrap();
        let request = CommitRequest {
            account: ACCOUNT,
            expected: Sequence::default(),
            utc_ms: 0,
            deadline: deadline(),
        };
        assert_eq!(
            store.commit(&td_crypto::Provider, request, &[op], &mut []),
            Ok(Sequence::from_u64(1))
        );
        timer.0.store(1, Ordering::Relaxed);
        assert_eq!(
            store.commit(&td_crypto::Provider, request, &[op], &mut []),
            Err(CommitError::Rejected(ports::Error::Conflict))
        );
        assert_eq!(
            store
                .view(ACCOUNT, deadline())
                .unwrap()
                .identity()
                .committed_sequence,
            Sequence::from_u64(1)
        );
        drop(store);
        let reopened = IndexStore::open(&mut root, timer, 1, deadline()).unwrap();
        let mut view = reopened.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(view.identity().committed_sequence, Sequence::from_u64(1));
        let mut value = [0; 128];
        assert!(matches!(
            view.get(Key::Mailbox(ID), &mut value).unwrap(),
            Some((
                Row::Mailbox(MailboxRow {
                    name: "committed",
                    ..
                }),
                _
            ))
        ));
    }
    fn body_row(bytes: &[u8], kind: BlobKind) -> BlobRow {
        let mut digest = td_crypto::Provider.sha256().unwrap();
        digest.update(bytes).unwrap();
        BlobRow {
            kind,
            length: bytes.len() as u64,
            digest: digest.finish().unwrap(),
            created_at: 0,
        }
    }
    fn request(sequence: u64) -> CommitRequest {
        CommitRequest {
            account: ACCOUNT,
            expected: Sequence::from_u64(sequence),
            utc_ms: 0,
            deadline: deadline(),
        }
    }
    #[test]
    fn database_bodies_are_verified_atomic_and_survive_old_snapshots() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let timer = Arc::new(Timer(AtomicU64::new(1)));
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            timer.clone(),
            2,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let id = BlobId::from_bytes([4; 16]);
        let raw = b"Subject: test\r\n\r\nbody\r\n";
        let row = body_row(raw, BlobKind::Message);
        let value = encode(Row::Blob(row));
        let op = Operation::put(Table::Blobs, id.as_bytes(), &value).unwrap();
        assert_eq!(
            store.commit(&td_crypto::Provider, request(0), &[op], &mut []),
            Err(CommitError::Rejected(ports::Error::Invalid))
        );
        for mut bad in [
            &raw[..raw.len() - 1],
            &raw[..0],
            b"incorrect digest bytes!".as_slice(),
        ] {
            assert_eq!(
                store.commit(
                    &td_crypto::Provider,
                    request(0),
                    &[op],
                    &mut [BlobSource {
                        id,
                        source: &mut bad
                    }]
                ),
                Err(CommitError::Rejected(ports::Error::Corrupt))
            );
            let mut view = store.view(ACCOUNT, deadline()).unwrap();
            assert!(view.get(Key::Blob(id), &mut [0; 64]).unwrap().is_none());
            assert_eq!(view.identity().committed_sequence, Sequence::default());
        }
        let wrong_digest = encode(Row::Blob(BlobRow {
            digest: [0; 32],
            ..row
        }));
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                request(0),
                &[Operation::put(Table::Blobs, id.as_bytes(), &wrong_digest).unwrap()],
                &mut [BlobSource {
                    id,
                    source: &mut raw.as_slice()
                }]
            ),
            Err(CommitError::Rejected(ports::Error::Corrupt))
        );
        let longer = [raw.as_slice(), b"extra"].concat();
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                request(0),
                &[op],
                &mut [BlobSource {
                    id,
                    source: &mut longer.as_slice()
                }]
            ),
            Err(CommitError::Rejected(ports::Error::Corrupt))
        );
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                request(0),
                &[op],
                &mut [BlobSource {
                    id,
                    source: &mut raw.as_slice()
                }]
            ),
            Ok(Sequence::from_u64(1))
        );
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        let mut input = view
            .open_blob_input(&td_crypto::Provider, id, row.length)
            .unwrap();
        let mut bytes = [0; 128];
        assert_eq!(input.read(&mut bytes).unwrap(), raw.len());
        let mut body = input.finish().unwrap();
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                request(1),
                &[Operation::delete(Table::Blobs, id.as_bytes()).unwrap()],
                &mut []
            ),
            Ok(Sequence::from_u64(2))
        );
        assert_eq!(
            ports::BlobReader::read_at(&mut body, 0, &mut bytes).unwrap(),
            raw.len()
        );
        assert_eq!(&bytes[..raw.len()], raw);
        {
            let mut fresh = store.view(ACCOUNT, deadline()).unwrap();
            assert!(fresh.get(Key::Blob(id), &mut [0; 64]).unwrap().is_none());
        }
        assert_eq!(store.checkpoint(deadline()), Err(ports::Error::Busy));
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                request(2),
                &[op],
                &mut [BlobSource {
                    id,
                    source: &mut raw.as_slice()
                }]
            ),
            Err(CommitError::Rejected(ports::Error::Conflict))
        );
        timer.0.store(100, Ordering::Relaxed);
        assert_eq!(body.check_deadline(), Err(ports::Error::Deadline));
        timer.0.store(1, Ordering::Relaxed);
        assert_eq!(body.check_deadline(), Err(ports::Error::Deadline));
        drop(body);
        drop(view);
        store.checkpoint(deadline()).unwrap();
        assert!(!fixture.path.join("accounts").exists());
    }

    #[test]
    fn native_database_full_rolls_back_body_metadata_and_id_registration() {
        use crate::{limits::SQLITE_BODY_CHUNK_BYTES, ports::BlobReader};
        const BODY_BYTES: usize = 2 * 1024 * 1024;
        fn verify(view: &mut IndexReadView<'_, '_>, id: BlobId, expected: &[u8]) {
            let mut input = view
                .open_blob_input(&td_crypto::Provider, id, expected.len() as u64)
                .unwrap();
            let mut scratch = [0; 4096];
            let mut position = 0;
            while position < expected.len() {
                let count = input.read(&mut scratch).unwrap();
                assert!(count > 0);
                assert_eq!(&scratch[..count], &expected[position..position + count]);
                position += count;
            }
            assert_eq!(input.finish().unwrap().len(), expected.len() as u64);
        }
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let clock = Arc::new(Timer(AtomicU64::new(1)));
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            clock.clone(),
            2,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let original = BlobId::from_bytes([0x91; 16]);
        let candidate = BlobId::from_bytes([0x92; 16]);
        let original_body = b"previous committed body";
        let original_row = encode(Row::Blob(body_row(original_body, BlobKind::Message)));
        let original_mailbox = mailbox("original", None);
        store
            .commit(
                &td_crypto::Provider,
                request(0),
                &[
                    Operation::put(Table::Blobs, original.as_bytes(), &original_row).unwrap(),
                    Operation::put(Table::Mailboxes, ID.as_bytes(), &original_mailbox).unwrap(),
                ],
                &mut [BlobSource {
                    id: original,
                    source: &mut original_body.as_slice(),
                }],
            )
            .unwrap();
        store.checkpoint(deadline()).unwrap();
        let mut snapshot = store.view(ACCOUNT, deadline()).unwrap();
        {
            let writer = lock(&store.writer).unwrap();
            let db = lock(&writer.native.connection).unwrap();
            let pages: i64 = db
                .pragma_query_value(None, "page_count", |r| r.get(0))
                .unwrap();
            let cap = pages + (BODY_BYTES as u64 / 2 / PAGE_BYTES) as i64;
            let actual: i64 = db
                .pragma_query_value(None, "max_page_count", |r| r.get(0))
                .unwrap();
            assert_eq!(actual, MAX_PAGES as i64);
            db.pragma_update(None, "max_page_count", cap).unwrap();
            let actual: i64 = db
                .pragma_query_value(None, "max_page_count", |r| r.get(0))
                .unwrap();
            assert_eq!(actual, cap);
            // Establish the native failure, not just the Capacity translation.
            db.execute_batch("BEGIN IMMEDIATE").unwrap();
            let chunk = [0x5a_u8; SQLITE_BODY_CHUNK_BYTES];
            let error = (1..=32)
                .find_map(|ordinal| {
                    db.execute(
                        "INSERT INTO blob_chunks(account,blob,ordinal,body) VALUES(?1,?2,?3,?4)",
                        params![
                            ACCOUNT.as_bytes().as_slice(),
                            original.as_bytes().as_slice(),
                            ordinal,
                            chunk.as_slice()
                        ],
                    )
                    .err()
                })
                .unwrap();
            assert!(matches!(error, rusqlite::Error::SqliteFailure(error, _)
                if error.code == rusqlite::ErrorCode::DiskFull));
            // This single-row insert exercises automatic transaction rollback.
            assert!(db.is_autocommit());
        }
        let body = vec![0x5a; BODY_BYTES];
        let row = encode(Row::Blob(body_row(&body, BlobKind::Message)));
        let changed_mailbox = mailbox("replacement", None);
        let operations = [
            Operation::put(Table::Mailboxes, ID.as_bytes(), &changed_mailbox).unwrap(),
            Operation::delete(Table::Blobs, original.as_bytes()).unwrap(),
            Operation::put(Table::Blobs, candidate.as_bytes(), &row).unwrap(),
        ];
        let mut source = std::io::Cursor::new(body.as_slice());
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                request(1),
                &operations,
                &mut [BlobSource {
                    id: candidate,
                    source: &mut source
                }],
            ),
            Err(CommitError::Rejected(ports::Error::Capacity))
        );
        assert!(source.position() > SQLITE_BODY_CHUNK_BYTES as u64);
        assert!(source.position() < body.len() as u64);
        {
            let writer = lock(&store.writer).unwrap();
            assert!(!writer.stopped);
            assert!(lock(&writer.native.connection).unwrap().is_autocommit());
            assert!(lock(&writer.native.budget).unwrap().failure.is_none());
        }
        verify(&mut snapshot, original, original_body);
        {
            let mut fresh = store.view(ACCOUNT, deadline()).unwrap();
            assert_eq!(fresh.identity().committed_sequence, Sequence::from_u64(1));
            let mut bytes = [0; 128];
            assert!(fresh
                .get(Key::Blob(candidate), &mut bytes)
                .unwrap()
                .is_none());
            assert!(matches!(
                fresh.get(Key::Mailbox(ID), &mut bytes).unwrap(),
                Some((
                    Row::Mailbox(MailboxRow {
                        name: "original",
                        ..
                    }),
                    _
                ))
            ));
            verify(&mut fresh, original, original_body);
        }
        {
            let writer = lock(&store.writer).unwrap();
            lock(&writer.native.connection)
                .unwrap()
                .pragma_update(None, "max_page_count", MAX_PAGES as i64)
                .unwrap();
        }
        source.set_position(0);
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                request(1),
                &operations,
                &mut [BlobSource {
                    id: candidate,
                    source: &mut source
                }],
            ),
            Ok(Sequence::from_u64(2))
        );
        assert_eq!(
            snapshot.identity().committed_sequence,
            Sequence::from_u64(1)
        );
        verify(&mut snapshot, original, original_body);
        drop(snapshot);
        drop(store);
        let reopened = IndexStore::open(&mut root, clock, 1, deadline()).unwrap();
        reopened.validate_integrity(deadline()).unwrap();
        let mut view = reopened.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(view.identity().committed_sequence, Sequence::from_u64(2));
        let mut bytes = [0; 128];
        assert!(view.get(Key::Blob(original), &mut bytes).unwrap().is_none());
        assert!(matches!(
            view.get(Key::Mailbox(ID), &mut bytes).unwrap(),
            Some((
                Row::Mailbox(MailboxRow {
                    name: "replacement",
                    ..
                }),
                _
            ))
        ));
        verify(&mut view, candidate, &body);
    }

    #[test]
    fn maximum_body_streams_with_fixed_scratch_and_reopens() {
        use std::io::Read;
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            Arc::new(Timer(AtomicU64::new(1))),
            1,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let id = BlobId::from_bytes([4; 16]);
        let chunk = [0x5a; super::super::MAX_FILE_STEP_BYTES];
        let mut digest = td_crypto::Provider.sha256().unwrap();
        for _ in 0..MAX_BODY_BYTES / chunk.len() as u64 {
            digest.update(&chunk).unwrap();
        }
        let value = encode(Row::Blob(BlobRow {
            kind: BlobKind::Message,
            length: MAX_BODY_BYTES,
            digest: digest.finish().unwrap(),
            created_at: 0,
        }));
        let op = Operation::put(Table::Blobs, id.as_bytes(), &value).unwrap();
        let mut source = std::io::repeat(0x5a).take(MAX_BODY_BYTES);
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                request(0),
                &[op],
                &mut [BlobSource {
                    id,
                    source: &mut source
                }]
            ),
            Ok(Sequence::from_u64(1))
        );
        assert_eq!(
            store.commit(&td_crypto::Provider, request(1), &[op], &mut []),
            Ok(Sequence::from_u64(2))
        );
        {
            let mut view = store.view(ACCOUNT, deadline()).unwrap();
            let mut row = [0; 64];
            assert_eq!(
                view.get(Key::Blob(id), &mut row).unwrap().unwrap().1,
                Sequence::from_u64(1)
            );
        }
        drop(store);
        let reopened =
            IndexStore::open(&mut root, Arc::new(Timer(AtomicU64::new(1))), 1, deadline()).unwrap();
        let mut view = reopened.view(ACCOUNT, deadline()).unwrap();
        let mut input = view
            .open_blob_input(&td_crypto::Provider, id, MAX_BODY_BYTES)
            .unwrap();
        let mut output = [0; super::super::MAX_FILE_STEP_BYTES];
        while input.position() < input.len() {
            let count = input.read(&mut output).unwrap();
            assert!(count > 0);
            assert!(output[..count].iter().all(|byte| *byte == 0x5a));
        }
        let mut pin = input.finish().unwrap();
        assert_eq!(
            ports::BlobReader::read_at(&mut pin, 4093, &mut output).unwrap(),
            output.len()
        );
        assert!(output.iter().all(|byte| *byte == 0x5a));
    }

    #[test]
    fn deadline_during_body_copy_rolls_back_content_and_metadata() {
        struct Expiring<'a> {
            timer: &'a Timer,
            bytes: &'a [u8],
        }
        impl std::io::Read for Expiring<'_> {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                let n = std::io::Read::read(&mut self.bytes, output)?;
                self.timer.0.store(100, Ordering::Relaxed);
                Ok(n)
            }
        }
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let timer = Arc::new(Timer(AtomicU64::new(1)));
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            timer.clone(),
            1,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let id = BlobId::from_bytes([4; 16]);
        let value = encode(Row::Blob(body_row(b"body", BlobKind::Message)));
        let op = Operation::put(Table::Blobs, id.as_bytes(), &value).unwrap();
        let mut expiring = Expiring {
            timer: &timer,
            bytes: b"body",
        };
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                request(0),
                &[op],
                &mut [BlobSource {
                    id,
                    source: &mut expiring
                }]
            ),
            Err(CommitError::Rejected(ports::Error::Deadline))
        );
        timer.0.store(1, Ordering::Relaxed);
        {
            let mut view = store.view(ACCOUNT, deadline()).unwrap();
            assert_eq!(view.identity().committed_sequence, Sequence::default());
            assert!(view.get(Key::Blob(id), &mut [0; 64]).unwrap().is_none());
        }
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                request(0),
                &[op],
                &mut [BlobSource {
                    id,
                    source: &mut b"body".as_slice()
                }]
            ),
            Ok(Sequence::from_u64(1))
        );
    }

    #[test]
    fn empty_body_and_stream_errors_preserve_atomic_metadata() {
        struct Broken;
        impl std::io::Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::StorageFull.into())
            }
        }
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            Arc::new(Timer(AtomicU64::new(1))),
            1,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let id = BlobId::from_bytes([4; 16]);
        let value = encode(Row::Blob(body_row(b"", BlobKind::Upload)));
        let mailbox = mailbox("must roll back", None);
        let ops = [
            Operation::put(Table::Mailboxes, ID.as_bytes(), &mailbox).unwrap(),
            Operation::put(Table::Blobs, id.as_bytes(), &value).unwrap(),
        ];
        assert!(matches!(
            store.commit(
                &td_crypto::Provider,
                request(0),
                &ops,
                &mut [BlobSource {
                    id,
                    source: &mut Broken
                }]
            ),
            Err(CommitError::Rejected(ports::Error::Io {
                kind: std::io::ErrorKind::StorageFull,
                ..
            }))
        ));
        {
            let mut view = store.view(ACCOUNT, deadline()).unwrap();
            assert!(view.get(Key::Mailbox(ID), &mut [0; 128]).unwrap().is_none());
            assert!(view.get(Key::Blob(id), &mut [0; 128]).unwrap().is_none());
        }
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                request(0),
                &ops,
                &mut [BlobSource {
                    id,
                    source: &mut b"".as_slice()
                }]
            ),
            Ok(Sequence::from_u64(1))
        );
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        let input = view.open_blob_input(&td_crypto::Provider, id, 0).unwrap();
        assert!(input.is_empty());
        assert_eq!(ports::BlobReader::len(&input.finish().unwrap()), 0);
    }

    #[test]
    fn reopen_refuses_a_different_sqlite_page_size() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let timer = Arc::new(Timer(AtomicU64::new(1)));
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            timer.clone(),
            1,
            deadline(),
        )
        .unwrap();
        drop(store);
        let db = Connection::open(fixture.path.join("metadata.sqlite3")).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode=DELETE; PRAGMA page_size=8192; VACUUM; PRAGMA journal_mode=WAL;",
        )
        .unwrap();
        let size: i64 = db
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .unwrap();
        assert_eq!(size, 8192);
        drop(db);
        assert!(matches!(
            IndexStore::open(&mut root, timer, 1, deadline()),
            Err(ports::Error::Corrupt)
        ));
    }

    #[test]
    fn create_refuses_preexisting_sidecars_before_creating_the_database() {
        for name in ["metadata.sqlite3-wal", "metadata.sqlite3-shm"] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            std::os::unix::fs::symlink(fixture.path.join("missing"), fixture.path.join(name))
                .unwrap();
            assert!(matches!(
                IndexStore::create(
                    &mut root,
                    StoreEpoch::from_bytes([9; 16]),
                    Arc::new(Timer(AtomicU64::new(1))),
                    1,
                    deadline(),
                ),
                Err(ports::Error::Invalid)
            ));
            assert!(!fixture.path.join("metadata.sqlite3").exists());
        }
    }

    #[test]
    fn retired_reader_slots_release_the_maintenance_fence() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            Arc::new(Timer(AtomicU64::new(1))),
            2,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let view = store.view(ACCOUNT, deadline()).unwrap();
        let poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = view.native().unwrap().connection.lock().unwrap();
            panic!("injected reader rollback lock failure");
        }));
        assert!(poison.is_err());
        drop(view);
        assert!(matches!(
            lock(&store.readers).unwrap().first(),
            Some(ReaderSlot::Retired)
        ));
        store.checkpoint(deadline()).unwrap();
        let healthy = store.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(healthy.identity().committed_sequence, Sequence::default());
    }

    #[test]
    fn deadline_before_commit_rolls_back_and_writer_contention_is_bounded() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let timer = Arc::new(Timer(AtomicU64::new(1)));
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            timer.clone(),
            1,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let mut writer = lock(&store.writer).unwrap();
        assert!(matches!(
            store.view(ACCOUNT, deadline()),
            Err(ports::Error::Busy)
        ));
        assert_eq!(store.checkpoint(deadline()), Err(ports::Error::Busy));
        writer.native.begin_work(deadline()).unwrap();
        writer
            .native
            .run(|db| db.execute_batch("BEGIN IMMEDIATE").map_err(sql))
            .unwrap();
        timer.0.store(100, Ordering::Relaxed);
        assert_eq!(
            finish_commit(&mut writer),
            Err(CommitError::Rejected(ports::Error::Deadline))
        );
        assert!(!writer.stopped);
        assert!(lock(&writer.native.connection).unwrap().is_autocommit());
        drop(writer);
        timer.0.store(1, Ordering::Relaxed);
        assert_eq!(
            store
                .view(ACCOUNT, deadline())
                .unwrap()
                .identity()
                .committed_sequence,
            Sequence::default()
        );
    }

    #[test]
    fn sqlite_error_domains_and_body_ceiling_remain_distinct() {
        for (native, expected) in [
            (
                rusqlite::ErrorCode::SystemIoFailure,
                ports::Error::Io {
                    kind: std::io::ErrorKind::Other,
                    os_code: None,
                },
            ),
            (rusqlite::ErrorCode::DatabaseCorrupt, ports::Error::Corrupt),
            (rusqlite::ErrorCode::OutOfMemory, ports::Error::Capacity),
            (rusqlite::ErrorCode::DatabaseBusy, ports::Error::Busy),
        ] {
            assert_eq!(
                sql(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error {
                        code: native,
                        extended_code: 0
                    },
                    None
                )),
                expected
            );
        }
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            Arc::new(Timer(AtomicU64::new(1))),
            1,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let id = BlobId::from_bytes([4; 16]);
        let value = encode(Row::Blob(BlobRow {
            kind: BlobKind::Message,
            length: MAX_BODY_BYTES + 1,
            digest: [0; 32],
            created_at: 0,
        }));
        let operation = Operation::put(Table::Blobs, id.as_bytes(), &value).unwrap();
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    expected: Sequence::default(),
                    utc_ms: 0,
                    deadline: deadline()
                },
                &[operation],
                &mut []
            ),
            Err(CommitError::Rejected(ports::Error::Capacity))
        );
    }

    #[test]
    fn commit_work_does_not_scan_unrelated_metadata() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            Arc::new(Timer(AtomicU64::new(1))),
            1,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        lock(&store.writer)
            .unwrap()
            .native
            .run(|db| {
                db.execute_batch("BEGIN IMMEDIATE").map_err(sql)?;
                let mut statement = db
                    .prepare("INSERT INTO mailboxes(account,id,name,parent_id,role,sort_order,subscribed,changed) VALUES(?1,?2,?3,NULL,NULL,0,1,?4)")
                    .map_err(sql)?;
                for i in 0_u64..5000 {
                    let mut key = [0; 16];
                    key.get_mut(..8).unwrap().copy_from_slice(&i.to_be_bytes());
                    statement
                        .execute(params![
                            ACCOUNT.as_bytes().as_slice(),
                            key.as_slice(),
                            "existing",
                            [0_u8; 8].as_slice()
                        ])
                        .map_err(sql)?;
                }
                drop(statement);
                db.execute_batch("COMMIT").map_err(sql)
            })
            .unwrap();
        let value = mailbox("changed", None);
        let op = Operation::put(Table::Mailboxes, ID.as_bytes(), &value).unwrap();
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    expected: Sequence::default(),
                    utc_ms: 0,
                    deadline: deadline()
                },
                &[op],
                &mut []
            ),
            Ok(Sequence::from_u64(1))
        );
        let writer = lock(&store.writer).unwrap();
        let consumed = VM_STEPS - lock(&writer.native.budget).unwrap().remaining;
        assert!(
            consumed < 10000,
            "unrelated rows consumed {consumed} VM steps"
        );
    }
    #[test]
    fn absent_change_kind_uses_its_native_index_without_statistics() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            Arc::new(Timer(AtomicU64::new(1))),
            1,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        lock(&store.writer)
            .unwrap()
            .native
            .run(|db| {
                db.execute_batch("BEGIN IMMEDIATE").map_err(sql)?;
                let mut statement = db
                    .prepare("INSERT INTO changes VALUES(?1,?2,0,?3,3,?4)")
                    .map_err(sql)?;
                for i in 1_u64..=5000 {
                    let kind = if i % 2 == 0 {
                        ObjectType::Email
                    } else {
                        ObjectType::Mailbox
                    };
                    statement
                        .execute(params![
                            ACCOUNT.as_bytes().as_slice(),
                            i.to_be_bytes().as_slice(),
                            kind.tag(),
                            ID.as_bytes().as_slice()
                        ])
                        .map_err(sql)?;
                }
                drop(statement);
                db.execute(
                    "UPDATE accounts SET sequence=?2 WHERE id=?1",
                    params![
                        ACCOUNT.as_bytes().as_slice(),
                        5000_u64.to_be_bytes().as_slice()
                    ],
                )
                .map_err(sql)?;
                db.execute_batch("COMMIT").map_err(sql)
            })
            .unwrap();
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        let before = lock(&view.native().unwrap().budget).unwrap().remaining;
        assert_eq!(
            view.next_change(
                ChangeCursor {
                    sequence: Sequence::default(),
                    operation: 0
                },
                ObjectType::Thread
            ),
            Ok(ChangeStep::Complete)
        );
        let consumed = before - lock(&view.native().unwrap().budget).unwrap().remaining;
        assert!(consumed < 100, "absent kind consumed {consumed} VM steps");
    }

    #[test]
    fn native_two_target_references_cascade_and_membership_index() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            Arc::new(Timer(AtomicU64::new(1))),
            1,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let blob = BlobId::from_bytes([4; 16]);
        let thread = ThreadId::from_bytes([5; 16]);
        let email = EmailId::from_bytes([6; 16]);
        let raw = b"Subject: references\r\n\r\nbody\r\n";
        let mut digest = td_crypto::Provider.sha256().unwrap();
        digest.update(raw).unwrap();
        let blob_value = encode(Row::Blob(BlobRow {
            kind: BlobKind::Message,
            length: raw.len() as u64,
            digest: digest.finish().unwrap(),
            created_at: 0,
        }));
        let thread_value = encode(Row::Thread);
        let email_value = encode(Row::Email(EmailRow {
            blob,
            thread,
            received_at: 0,
            origin: EmailOrigin::Jmap,
        }));
        let mailbox_value = mailbox("inbox", None);
        let membership_value = encode(Row::Membership);
        let mut member_key = [0; 32];
        Key::Membership(email, ID).encode(&mut member_key).unwrap();
        let request = |sequence| CommitRequest {
            account: ACCOUNT,
            expected: Sequence::from_u64(sequence),
            utc_ms: 0,
            deadline: deadline(),
        };
        let ops = [
            Operation::put(Table::Blobs, blob.as_bytes(), &blob_value).unwrap(),
            Operation::put(Table::Threads, thread.as_bytes(), &thread_value).unwrap(),
            Operation::put(Table::Emails, email.as_bytes(), &email_value).unwrap(),
            Operation::put(Table::Mailboxes, ID.as_bytes(), &mailbox_value).unwrap(),
            Operation::put(Table::Memberships, &member_key, &membership_value).unwrap(),
        ];
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                request(0),
                &ops,
                &mut [BlobSource {
                    id: blob,
                    source: &mut raw.as_slice()
                }]
            ),
            Ok(Sequence::from_u64(1))
        );
        lock(&store.writer)
            .unwrap()
            .native
            .run(|db| {
                let found: Vec<u8> = db
                    .query_row(
                        MEMBERSHIP_EMAIL,
                        params![ACCOUNT.as_bytes().as_slice(), ID.as_bytes().as_slice()],
                        |row| row.get(0),
                    )
                    .map_err(sql)?;
                assert_eq!(found, email.as_bytes());
                let count: i64 = db
                    .query_row(
                        "SELECT count(*) FROM memberships WHERE account=?1",
                        [ACCOUNT.as_bytes().as_slice()],
                        |row| row.get(0),
                    )
                    .map_err(sql)?;
                assert_eq!(count, 1);
                Ok(())
            })
            .unwrap();
        for (table, key) in [
            (Table::Blobs, blob.as_bytes()),
            (Table::Threads, thread.as_bytes()),
            (Table::Emails, email.as_bytes()),
            (Table::Mailboxes, ID.as_bytes()),
        ] {
            assert_eq!(
                store.commit(
                    &td_crypto::Provider,
                    request(1),
                    &[Operation::delete(table, key).unwrap()],
                    &mut []
                ),
                Err(CommitError::Rejected(ports::Error::Conflict))
            );
        }
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                request(1),
                &[
                    Operation::delete(Table::Memberships, &member_key).unwrap(),
                    Operation::delete(Table::Emails, email.as_bytes()).unwrap(),
                    Operation::delete(Table::Threads, thread.as_bytes()).unwrap(),
                    Operation::delete(Table::Blobs, blob.as_bytes()).unwrap(),
                ],
                &mut []
            ),
            Ok(Sequence::from_u64(2))
        );
        lock(&store.writer)
            .unwrap()
            .native
            .run(|db| {
                let count: i64 = db
                    .query_row(
                        "SELECT count(*) FROM memberships WHERE account=?1",
                        [ACCOUNT.as_bytes().as_slice()],
                        |row| row.get(0),
                    )
                    .map_err(sql)?;
                assert_eq!(count, 0);
                Ok(())
            })
            .unwrap();
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        let mut bytes = [0; 128];
        assert!(view.get(Key::Email(email), &mut bytes).unwrap().is_none());
        assert!(view.get(Key::Mailbox(ID), &mut bytes).unwrap().is_some());
    }

    #[test]
    fn native_lease_references_follow_the_commit_utc_boundary() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            Arc::new(Timer(AtomicU64::new(1))),
            1,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let blob = BlobId::from_bytes([4; 16]);
        let mut digest = td_crypto::Provider.sha256().unwrap();
        digest.update(b"upload").unwrap();
        let blob_value = encode(Row::Blob(BlobRow {
            kind: BlobKind::Upload,
            length: 6,
            digest: digest.finish().unwrap(),
            created_at: 0,
        }));
        let lease_value = encode(Row::Lease(LeaseRow {
            account: ACCOUNT,
            device: DeviceId::from_bytes([5; 16]),
            expires_at: 10,
            uses: LeaseUse::Both,
        }));
        let request = |sequence, utc_ms| CommitRequest {
            account: ACCOUNT,
            expected: Sequence::from_u64(sequence),
            utc_ms,
            deadline: deadline(),
        };
        let lease = Operation::put(Table::Leases, blob.as_bytes(), &lease_value).unwrap();
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                request(0, 9),
                &[
                    Operation::put(Table::Blobs, blob.as_bytes(), &blob_value).unwrap(),
                    lease,
                ],
                &mut [BlobSource {
                    id: blob,
                    source: &mut b"upload".as_slice()
                }]
            ),
            Ok(Sequence::from_u64(1))
        );
        let delete = Operation::delete(Table::Blobs, blob.as_bytes()).unwrap();
        assert_eq!(
            store.commit(&td_crypto::Provider, request(1, 9), &[delete], &mut []),
            Err(CommitError::Rejected(ports::Error::Conflict))
        );
        // Expiry never bypasses the foreign key: explicit lease deletion releases content.
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                request(1, 10),
                &[
                    Operation::delete(Table::Leases, blob.as_bytes()).unwrap(),
                    delete
                ],
                &mut []
            ),
            Ok(Sequence::from_u64(2))
        );
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        let mut bytes = [0; 128];
        assert!(view.get(Key::Blob(blob), &mut bytes).unwrap().is_none());
        assert!(view.get(Key::Lease(blob), &mut bytes).unwrap().is_none());
    }

    #[test]
    fn wal_reservation_checks_its_exact_native_transaction_headroom() {
        let fixture = Fixture::new();
        let root = fixture.locked();
        let path = db_path(&root, RootEntry::Wal).unwrap();
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .unwrap();
        file.set_len(MAX_WAL_BYTES - TRANSACTION_WAL_BYTES).unwrap();
        assert_eq!(reserve_wal(&root), Ok(()));
        file.set_len(MAX_WAL_BYTES - TRANSACTION_WAL_BYTES + 1)
            .unwrap();
        assert_eq!(reserve_wal(&root), Err(ports::Error::Busy));
        file.set_len(MAX_WAL_BYTES + 1).unwrap();
        assert_eq!(reserve_wal(&root), Err(ports::Error::Invalid));
    }

    #[test]
    fn late_creation_deadline_can_leave_a_valid_database_for_fresh_open() {
        struct AfterPublication(std::path::PathBuf);
        impl Clock for AfterPublication {
            fn sample(&self) -> Result<Time, ports::Error> {
                use std::os::unix::fs::FileExt;
                let committed = fs::File::open(&self.0).is_ok_and(|file| {
                    let length = file.metadata().unwrap().len();
                    let mut offset = 32_u64;
                    let mut size = [0; 4];
                    while offset + 24 + PAGE_BYTES <= length {
                        file.read_exact_at(&mut size, offset + 4).unwrap();
                        if u32::from_be_bytes(size) != 0 {
                            return true;
                        }
                        offset += 24 + PAGE_BYTES;
                    }
                    false
                });
                let tick = if committed { 100 } else { 1 };
                Ok(Time {
                    utc_ms: 0,
                    monotonic: Tick(tick),
                })
            }
        }
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let wal = db_path(&root, RootEntry::Wal).unwrap();
        assert!(matches!(
            IndexStore::create(
                &mut root,
                StoreEpoch::from_bytes([9; 16]),
                Arc::new(AfterPublication(wal)),
                1,
                deadline()
            ),
            Err(ports::Error::Deadline)
        ));
        let store =
            IndexStore::open(&mut root, Arc::new(Timer(AtomicU64::new(1))), 1, deadline()).unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let view = store.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(view.identity().epoch, StoreEpoch::from_bytes([9; 16]));
    }

    #[test]
    fn integrity_scan_refuses_a_stopped_writer_without_reading_it() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            Arc::new(Timer(AtomicU64::new(1))),
            1,
            deadline(),
        )
        .unwrap();
        let before;
        {
            let mut writer = lock(&store.writer).unwrap();
            writer.stopped = true;
            before = lock(&writer.native.budget).unwrap().remaining;
        }
        assert_eq!(
            store.validate_integrity(deadline()),
            Err(ports::Error::WriterStopped)
        );
        assert_eq!(
            lock(&lock(&store.writer).unwrap().native.budget)
                .unwrap()
                .remaining,
            before
        );
    }

    #[test]
    fn opening_does_not_scan_rows_and_integrity_is_explicit_maintenance() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let clock: Arc<dyn Clock> = Arc::new(Timer(AtomicU64::new(1)));
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            clock.clone(),
            1,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        store.validate_integrity(deadline()).unwrap();
        {
            let writer = lock(&store.writer).unwrap();
            writer
                .native
                .run(|db| {
                    db.execute_batch("PRAGMA foreign_keys=OFF").map_err(sql)?;
                    db.execute(
                        "INSERT INTO keywords VALUES(?1,?2,'bad',?3)",
                        params![
                            ACCOUNT.as_bytes().as_slice(),
                            [7u8; 16].as_slice(),
                            1u64.to_be_bytes().as_slice()
                        ],
                    )
                    .map_err(sql)?;
                    db.execute_batch("PRAGMA foreign_keys=ON").map_err(sql)
                })
                .unwrap();
        }
        drop(store);
        let store = IndexStore::open(&mut root, clock, 1, deadline()).unwrap();
        assert_eq!(
            store.validate_integrity(deadline()),
            Err(ports::Error::Corrupt)
        );
    }

    #[test]
    fn restrictive_created_database_modes_are_restored_before_sqlite_open() {
        for mode in [0o000, 0o200] {
            let fixture = Fixture::new();
            let root = fixture.locked();
            let path = db_path(&root, RootEntry::Database).unwrap();
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&path)
                .unwrap();
            file.set_permissions(fs::Permissions::from_mode(mode))
                .unwrap();
            assert_eq!(file.metadata().unwrap().mode() & 0o777, mode);
            initialize_database_file(&file).unwrap();
            assert_eq!(file.metadata().unwrap().mode() & 0o7777, 0o600);
            drop(file);
            let native =
                Native::open(&path, Arc::new(Timer(AtomicU64::new(1))), deadline()).unwrap();
            native
                .run(|db| {
                    db.execute_batch(
                        "PRAGMA journal_mode=WAL; CREATE TABLE fixture(value INTEGER);",
                    )
                    .map_err(sql)
                })
                .unwrap();
            for name in [RootEntry::Database, RootEntry::Wal, RootEntry::SharedMemory] {
                assert_eq!(
                    fs::metadata(db_path(&root, name).unwrap()).unwrap().mode() & 0o7777,
                    0o600
                );
            }
        }
    }

    #[test]
    fn native_reference_and_parent_read_failures_preserve_their_domain() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        for select in [3, 4] {
            for error in [
                ports::Error::Deadline,
                ports::Error::Capacity,
                ports::Error::Io {
                    kind: std::io::ErrorKind::Other,
                    os_code: None,
                },
                ports::Error::Corrupt,
            ] {
                let fixture = Fixture::new();
                let mut root = fixture.locked();
                let store = IndexStore::create(
                    &mut root,
                    StoreEpoch::from_bytes([9; 16]),
                    Arc::new(Timer(AtomicU64::new(1))),
                    1,
                    deadline(),
                )
                .unwrap();
                store.create_account(ACCOUNT, deadline()).unwrap();
                let parent = mailbox("parent", None);
                let op = Operation::put(Table::Mailboxes, ID.as_bytes(), &parent).unwrap();
                let mut request = CommitRequest {
                    account: ACCOUNT,
                    expected: Sequence::default(),
                    utc_ms: 0,
                    deadline: deadline(),
                };
                store
                    .commit(&td_crypto::Provider, request, &[op], &mut [])
                    .unwrap();
                request.expected = Sequence::from_u64(1);
                let child_id = MailboxId::from_bytes([3; 16]);
                let child = mailbox("child", Some(ID));
                let op = Operation::put(Table::Mailboxes, child_id.as_bytes(), &child).unwrap();
                let seen = Arc::new(AtomicU64::new(0));
                {
                    let writer = lock(&store.writer).unwrap();
                    let budget = Arc::clone(&writer.native.budget);
                    let reads = Arc::clone(&seen);
                    lock(&writer.native.connection)
                        .unwrap()
                        .authorizer(Some(move |context: AuthContext<'_>| {
                            if matches!(context.action, AuthAction::Select)
                                && reads.fetch_add(1, Ordering::Relaxed) + 1 == select
                            {
                                lock(&budget).unwrap().failure = Some(error);
                            }
                            Authorization::Allow
                        }))
                        .unwrap();
                }
                // SELECT 1 captures account identity, 2 fetches the source;
                // 3 is reference target lookup, 4 is the parent walk's first get.
                assert_eq!(
                    store.commit(&td_crypto::Provider, request, &[op], &mut []),
                    Err(CommitError::Rejected(error))
                );
                assert_eq!(seen.load(Ordering::Relaxed), select);
                lock(&lock(&store.writer).unwrap().native.connection)
                    .unwrap()
                    .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                    .unwrap();
                let mut view = store.view(ACCOUNT, deadline()).unwrap();
                assert_eq!(view.identity().committed_sequence, Sequence::from_u64(1));
                let mut value = [0; 128];
                assert!(view
                    .get(Key::Mailbox(child_id), &mut value)
                    .unwrap()
                    .is_none());
                assert!(view.get(Key::Mailbox(ID), &mut value).unwrap().is_some());
                drop(view);
                assert_eq!(
                    store.commit(&td_crypto::Provider, request, &[op], &mut []),
                    Ok(Sequence::from_u64(2))
                );
            }
        }
    }

    #[test]
    fn deferred_constraint_remains_rejected_when_commit_clock_expires() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let timer = Arc::new(Timer(AtomicU64::new(1)));
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            timer.clone(),
            1,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        let parent = mailbox("parent", None);
        let child_id = MailboxId::from_bytes([3; 16]);
        let child = mailbox("child", Some(ID));
        let parent_op = Operation::put(Table::Mailboxes, ID.as_bytes(), &parent).unwrap();
        let child_op = Operation::put(Table::Mailboxes, child_id.as_bytes(), &child).unwrap();
        let mut request = CommitRequest {
            account: ACCOUNT,
            expected: Sequence::default(),
            utc_ms: 0,
            deadline: deadline(),
        };
        store
            .commit(
                &td_crypto::Provider,
                request,
                &[parent_op, child_op],
                &mut [],
            )
            .unwrap();
        request.expected = Sequence::from_u64(1);
        let hook = timer.clone();
        lock(&lock(&store.writer).unwrap().native.connection).unwrap().authorizer(Some(
            move |context: AuthContext<'_>| {
                // The sole transaction command besides BEGIN and ROLLBACK is COMMIT.
                if matches!(context.action, AuthAction::Transaction { operation }
                    if !matches!(operation, TransactionOperation::Begin | TransactionOperation::Rollback)) {
                    hook.0.store(100, Ordering::Relaxed);
                }
                Authorization::Allow
            }
        )).unwrap();
        let delete = Operation::delete(Table::Mailboxes, ID.as_bytes()).unwrap();
        assert_eq!(
            store.commit(&td_crypto::Provider, request, &[delete], &mut []),
            Err(CommitError::Rejected(ports::Error::Conflict))
        );
        assert_eq!(timer.0.load(Ordering::Relaxed), 100);
        assert!(!lock(&store.writer).unwrap().stopped);
        timer.0.store(1, Ordering::Relaxed);
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(view.identity().committed_sequence, Sequence::from_u64(1));
        let mut value = [0; 128];
        assert!(view.get(Key::Mailbox(ID), &mut value).unwrap().is_some());
    }

    #[test]
    fn unclassified_native_commit_failure_retires_writes_not_reads() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            Arc::new(Timer(AtomicU64::new(1))),
            1,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        lock(&lock(&store.writer).unwrap().native.connection).unwrap().authorizer(Some(
            |context: AuthContext<'_>| {
                if matches!(context.action, AuthAction::Transaction { operation }
                    if !matches!(operation, TransactionOperation::Begin | TransactionOperation::Rollback)) {
                    Authorization::Deny
                } else { Authorization::Allow }
            }
        )).unwrap();
        let value = mailbox("candidate", None);
        let op = Operation::put(Table::Mailboxes, ID.as_bytes(), &value).unwrap();
        let request = CommitRequest {
            account: ACCOUNT,
            expected: Sequence::default(),
            utc_ms: 0,
            deadline: deadline(),
        };
        assert_eq!(
            store.commit(&td_crypto::Provider, request, &[op], &mut []),
            Err(CommitError::Indeterminate(ports::Error::Io {
                kind: std::io::ErrorKind::Other,
                os_code: None
            }))
        );
        assert_eq!(
            store.commit(&td_crypto::Provider, request, &[op], &mut []),
            Err(CommitError::Rejected(ports::Error::WriterStopped))
        );
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        let mut bytes = [0; 128];
        assert_eq!(view.identity().committed_sequence, Sequence::default());
        assert!(view.get(Key::Mailbox(ID), &mut bytes).unwrap().is_none());
        drop(view);
        drop(store);
        let reopened =
            IndexStore::open(&mut root, Arc::new(Timer(AtomicU64::new(1))), 1, deadline()).unwrap();
        assert_eq!(
            reopened.commit(&td_crypto::Provider, request, &[op], &mut []),
            Ok(Sequence::from_u64(1))
        );
    }
}
