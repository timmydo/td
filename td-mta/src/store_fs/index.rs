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
#[cfg(test)]
use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
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
const MAX_ACCOUNTS: u32 = 128;
const MAX_OPERATIONS: usize = 4096;
const MAX_TRANSACTION_BYTES: usize = 1_048_576;
const VM_STEPS: u64 = 8_000_000;
const INTEGRITY_VM_STEPS: u64 = MAX_PAGES * PAGE_BYTES * 128;
const MAX_SHM_BYTES: u64 = crate::limits::SQLITE_WAL_INDEX_BYTES as u64;
const MAX_BODY_BYTES: u64 = crate::limits::MAX_MESSAGE_BYTES as u64;
const APP_ID: i64 = 0x54444d41;
const SECOND_ANCHOR: &str = "SELECT EXISTS(SELECT 1 FROM thread_anchors INDEXED BY anchors_email WHERE account=?1 AND email_id=?2 LIMIT 1 OFFSET 1)";
const NEXT_CHANGE: &str = "SELECT sequence,operation,action,object FROM changes INDEXED BY changes_kind WHERE account=?1 AND kind=?2 AND (sequence>?3 OR (sequence=?3 AND operation>?4)) AND sequence<=?5 ORDER BY sequence,operation LIMIT 1";
#[path = "index/relational.rs"]
mod relational;
use relational::SCHEMA;
#[path = "index/operations.rs"]
mod operations;
#[path = "index/row_history.rs"]
mod row_history;
use operations::Operations;
#[path = "index/history.rs"]
mod history;
#[path = "index/threading.rs"]
mod threading;
pub use history::{HistoryPruneRequest, HistoryPruned};
#[path = "index/usage.rs"]
mod usage;
pub use usage::LogicalUsage;
#[path = "index/usage_fence.rs"]
mod usage_fence;
pub use usage_fence::{
    AuxiliaryUsage, LedgerInitError, StoreFileUsage, StoreLogicalUsage, UsageFence,
};
#[path = "index/backup.rs"]
mod backup;
#[cfg(test)]
#[path = "index/crash_tests.rs"]
mod crash_tests;
#[cfg(test)]
#[path = "index/database_qualification.rs"]
mod database_qualification;
#[path = "index/epoch.rs"]
mod epoch;
#[cfg(test)]
#[path = "index/recipient_tests.rs"]
mod recipient_tests;
#[cfg(test)]
#[path = "index/wal_qualification.rs"]
mod wal_qualification;
pub use backup::{BackupError, BackupReceipt};

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
fn fixed_blob<const N: usize>(
    row: &rusqlite::Row<'_>,
    column: usize,
) -> Result<[u8; N], ports::Error> {
    let ValueRef::Blob(bytes) = row.get_ref(column).map_err(sql)? else {
        return Err(ports::Error::Corrupt);
    };
    bytes.try_into().map_err(|_| ports::Error::Corrupt)
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
    snapshot_failure: Mutex<Option<ports::Error>>,
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
            snapshot_failure: Mutex::new(None),
        })
    }
    fn close(self) -> Result<(), ports::Error> {
        let connection = self
            .connection
            .into_inner()
            .map_err(|_| ports::Error::WriterStopped)?;
        connection
            .close()
            .map_err(|(_connection, error)| sql(error))
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
    fn begin_work_after(
        &self,
        deadline: Deadline,
        previous: ports::Tick,
    ) -> Result<(), ports::Error> {
        self.begin_work(deadline)?;
        let mut budget = lock(&self.budget)?;
        if budget.last < previous {
            budget.failure = Some(ports::Error::Invalid);
            return Err(ports::Error::Invalid);
        }
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
    pub(in crate::store_fs) fn read_snapshot<T>(
        &self,
        read: impl FnOnce(&Native) -> Result<T, ports::Error>,
    ) -> Result<T, ports::Error> {
        if let Some(error) = *lock(&self.snapshot_failure)? {
            return Err(error);
        }
        let native = self;
        // SQLite may end a read transaction after NOMEM, IOERR or FULL.
        let live = lock(&native.connection).map(|db| !db.is_autocommit());
        if !matches!(live, Ok(true)) {
            let error = live.err().unwrap_or(ports::Error::Corrupt);
            *lock(&self.snapshot_failure)? = Some(error);
            return Err(error);
        }
        let result = read(native);
        let live = lock(&native.connection).map(|db| !db.is_autocommit());
        if !matches!(live, Ok(true)) {
            let error = live
                .err()
                .or_else(|| result.as_ref().err().copied())
                .unwrap_or(ports::Error::Corrupt);
            *lock(&self.snapshot_failure)? = Some(error);
            return Err(error);
        }
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
    /// Store epoch captured when planning this transaction.
    pub epoch: StoreEpoch,
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
    fn writer_observed(
        &self,
        deadline: Deadline,
    ) -> Result<(MutexGuard<'_, Writer>, ports::Tick), ports::Error> {
        let observed = self.clock.sample()?.monotonic;
        if deadline.expired(observed) {
            return Err(ports::Error::Deadline);
        }
        let writer = self.writer.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => ports::Error::Busy,
            TryLockError::Poisoned(_) => ports::Error::WriterStopped,
        })?;
        Ok((writer, observed))
    }
    pub fn root(&self) -> &LockedRoot {
        self.root
    }
    pub fn epoch(&self) -> StoreEpoch {
        self.epoch
    }
    /// Cold enumeration under the writer fence and ordinary native work limits.
    /// IDs are historical observations; callers quiesce mutations for later use.
    pub fn account_ids(&self, deadline: Deadline) -> Result<Vec<AccountId>, ports::Error> {
        let (writer, acquired) = self.writer_observed(deadline)?;
        if writer.stopped {
            return Err(ports::Error::WriterStopped);
        }
        writer.native.begin_work_after(deadline, acquired)?;
        writer.native.run(|db| {
            let mut statement = db
                .prepare("SELECT id FROM accounts ORDER BY id")
                .map_err(sql)?;
            let mut rows = statement.query([]).map_err(sql)?;
            let mut accounts = Vec::new();
            while let Some(row) = rows.next().map_err(sql)? {
                writer.native.check()?;
                if accounts.len() >= MAX_ACCOUNTS as usize {
                    return Err(ports::Error::Capacity);
                }
                accounts.push(AccountId::from_bytes(fixed_blob(row, 0)?));
            }
            Ok(accounts)
        })
    }
    pub fn create_account(
        &self,
        account: AccountId,
        deadline: Deadline,
    ) -> Result<(), CommitError> {
        let (mut writer, acquired) = self
            .writer_observed(deadline)
            .map_err(CommitError::Rejected)?;
        if writer.stopped {
            return Err(CommitError::Rejected(ports::Error::WriterStopped));
        }
        writer
            .native
            .begin_work_after(deadline, acquired)
            .map_err(CommitError::Rejected)?;
        reserve_wal(self.root).map_err(CommitError::Rejected)?;
        let result = writer.native.run(|db| {
            let count: i64 = db
                .query_row("SELECT count(*) FROM accounts", [], |r| r.get(0))
                .map_err(sql)?;
            if count >= i64::from(MAX_ACCOUNTS) {
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
        self.capture_view(account, deadline, VM_STEPS)
    }
    /// Cold verification view; one finite maintenance allowance covers its lifetime.
    pub fn maintenance_view(
        &self,
        account: AccountId,
        deadline: Deadline,
    ) -> Result<IndexReadView<'_, 'r>, ports::Error> {
        self.capture_view(account, deadline, INTEGRITY_VM_STEPS)
    }
    fn capture_view(
        &self,
        account: AccountId,
        deadline: Deadline,
        steps: u64,
    ) -> Result<IndexReadView<'_, 'r>, ports::Error> {
        let (_writer, acquired) = self.writer_observed(deadline)?;
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
        let result = native.begin_work_after(deadline, acquired).and_then(|()| {
            lock(&native.budget)?.remaining = steps;
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
        self.commit_operations(crypto, request, Operations::Typed(operations), sources)
    }
    /// Commit original validated encoded input through the same writer transaction.
    /// The caller retains input/slot admission and supplies only prepared body reads.
    pub fn commit_batch<C: ports::Crypto>(
        &self,
        crypto: &C,
        request: CommitRequest,
        batch: &crate::format::batch::Batch<'_, '_>,
        sources: &mut [BlobSource<'_>],
    ) -> Result<Sequence, CommitError> {
        self.commit_operations(crypto, request, Operations::Encoded(batch), sources)
    }
    fn commit_operations<C: ports::Crypto>(
        &self,
        crypto: &C,
        request: CommitRequest,
        operations: Operations<'_, '_>,
        sources: &mut [BlobSource<'_>],
    ) -> Result<Sequence, CommitError> {
        self.transaction(request.deadline, |native, scratch| {
            self.apply(native, crypto, request, operations, sources, scratch)
        })
    }
    fn transaction<T>(
        &self,
        deadline: Deadline,
        apply: impl FnOnce(&Native, &mut [u8]) -> Result<T, ports::Error>,
    ) -> Result<T, CommitError> {
        let (mut writer, acquired) = self
            .writer_observed(deadline)
            .map_err(CommitError::Rejected)?;
        if writer.stopped {
            return Err(CommitError::Rejected(ports::Error::WriterStopped));
        }
        writer
            .native
            .begin_work_after(deadline, acquired)
            .map_err(CommitError::Rejected)?;
        let Writer {
            native, scratch, ..
        } = &mut *writer;
        let result = apply(native, scratch);
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
        operations: Operations<'_, '_>,
        sources: &mut [BlobSource<'_>],
        scratch: &mut [u8],
    ) -> Result<Sequence, ports::Error> {
        if request.epoch != self.epoch {
            return Err(ports::Error::Conflict);
        }
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
            native.check()?;
            if sources
                .get(..ordinal)
                .ok_or(ports::Error::Invalid)?
                .iter()
                .any(|other| other.id == source.id)
            {
                return Err(ports::Error::Invalid);
            }
            let mut matches = 0usize;
            for position in 0..operations.len() {
                if operations.blob_put(position)? == Some(source.id) {
                    matches = matches.checked_add(1).ok_or(ports::Error::Capacity)?;
                    if matches > 1 {
                        return Err(ports::Error::Invalid);
                    }
                }
            }
            if matches != 1 {
                return Err(ports::Error::Invalid);
            }
        }
        let bytes = (0..operations.len()).try_fold(0usize, |n, position| {
            native.check()?;
            let op = operations.get(position)?;
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
        for position in 0..operations.len() {
            native.check()?;
            let op = operations.get(position)?;
            if let Value::Change(change) = op.value() {
                let key = change_key(change)?;
                let existed = view.get(key, scratch)?.is_some();
                if (change.action == ChangeAction::Created) == existed {
                    return Err(ports::Error::Invalid);
                }
            }
        }
        row_history::validate(native, &mut view, operations, scratch)?;
        for ordinal in 0..operations.len() {
            native.check()?;
            let op = operations.get(ordinal)?;
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
        for position in 0..operations.len() {
            native.check()?;
            let op = operations.get(position)?;
            if let Value::Change(change) = op.value() {
                let exists = view.get(change_key(change)?, scratch)?.is_some();
                if (change.action == ChangeAction::Destroyed) == exists {
                    return Err(ports::Error::Invalid);
                }
            }
        }
        for position in 0..operations.len() {
            native.check()?;
            let op = operations.get(position)?;
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
            if let Key::ThreadAnchor(_, email) = key {
                let duplicate: bool = native.run(|db| {
                    db.query_row(
                        SECOND_ANCHOR,
                        params![account.as_bytes().as_slice(), email.as_bytes().as_slice()],
                        |row| row.get(0),
                    )
                    .map_err(sql)
                })?;
                if duplicate {
                    return Err(ports::Error::Conflict);
                }
            }
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
        validate_recipients(&mut view, operations, scratch)?;
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
        let (writer, acquired) = self.writer_observed(deadline)?;
        if writer.stopped {
            return Err(ports::Error::WriterStopped);
        }
        writer.native.begin_work_after(deadline, acquired)?;
        lock(&writer.native.budget)?.remaining = INTEGRITY_VM_STEPS;
        writer.native.run(|db| {
            let check: String = db
                .query_row("PRAGMA integrity_check(1)", [], |row| row.get(0))
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
            drop(statement);
            let duplicate: bool = db
                .query_row(ANCHOR_CARDINALITY, [], |row| row.get(0))
                .map_err(sql)?;
            if duplicate {
                return Err(ports::Error::Corrupt);
            }
            let malformed: bool = db
                .query_row(BODY_GEOMETRY, [], |row| row.get(0))
                .map_err(sql)?;
            if malformed {
                return Err(ports::Error::Corrupt);
            }
            Ok(())
        })
    }
    /// Reclaim WAL only when no view owns a SQLite read transaction.
    pub fn checkpoint(&self, deadline: Deadline) -> Result<(), ports::Error> {
        self.checkpoint_after(deadline, None).map(|_| ())
    }
    fn checkpoint_after(
        &self,
        deadline: Deadline,
        previous: Option<ports::Tick>,
    ) -> Result<ports::Tick, ports::Error> {
        let (writer, acquired) = self.writer_observed(deadline)?;
        if previous.is_some_and(|previous| acquired < previous) {
            return Err(ports::Error::Invalid);
        }
        if writer.stopped {
            return Err(ports::Error::WriterStopped);
        }
        if lock(&self.readers)?
            .iter()
            .any(|slot| matches!(slot, ReaderSlot::Borrowed))
        {
            return Err(ports::Error::Busy);
        }
        writer.native.begin_work_after(deadline, acquired)?;
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
        let observed = lock(&writer.native.budget)?.last;
        Ok(observed)
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
const ANCHOR_CARDINALITY: &str = "SELECT EXISTS(
    SELECT 1 FROM thread_anchors INDEXED BY anchors_email
    GROUP BY account,email_id HAVING count(*)>1)";
const BODY_GEOMETRY: &str = "SELECT EXISTS(
    SELECT 1 FROM blobs b
    WHERE (SELECT count(*) FROM blob_chunks c WHERE c.account=b.account AND c.blob=b.id)
        != (b.length+65535)/65536
    OR EXISTS(SELECT 1 FROM blob_chunks c WHERE c.account=b.account AND c.blob=b.id
        AND (c.ordinal<0 OR c.ordinal>=(b.length+65535)/65536
            OR length(c.body)!=min(65536,b.length-c.ordinal*65536))))";
const BODY_EXTENT: &str = "SELECT
    EXISTS(SELECT 1 FROM blob_chunks c WHERE c.account=b.account AND c.blob=b.id AND c.ordinal<0),
    EXISTS(SELECT 1 FROM blob_chunks c WHERE c.account=b.account AND c.blob=b.id AND c.ordinal>=?2)
    FROM blobs b WHERE b.rowid=?1";

impl Native {
    pub(in crate::store_fs) fn verify_body_extent(
        &self,
        rowid: i64,
        length: u64,
    ) -> Result<(), ports::Error> {
        let chunks = length.div_ceil(super::MAX_FILE_STEP_BYTES as u64);
        self.run(|db| {
            let (before, after): (bool, bool) = db
                .query_row(
                    BODY_EXTENT,
                    params![
                        rowid,
                        i64::try_from(chunks).map_err(|_| ports::Error::Corrupt)?
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(sql)?;
            if before || after {
                return Err(ports::Error::Corrupt);
            }
            Ok(())
        })
    }
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
    validated_file_length(root, &fs::symlink_metadata(path)?, maximum).map(|_| ())
}
fn validated_file_length(
    root: &LockedRoot,
    metadata: &fs::Metadata,
    maximum: u64,
) -> Result<u64, ports::Error> {
    let owner = root.root.directory.metadata()?.uid();
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.nlink() != 1
        || metadata.mode() & 0o7777 != 0o600
        || metadata.len() > maximum
    {
        return Err(ports::Error::Invalid);
    }
    Ok(metadata.len())
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
    let committed_sequence = sequence(&fixed_blob::<8>(row, 0)?)?;
    let history_floor = sequence(&fixed_blob::<8>(row, 1)?)?;
    if history_floor > committed_sequence {
        return Err(ports::Error::Corrupt);
    }
    Ok(ViewIdentity {
        account,
        epoch,
        committed_sequence,
        history_floor,
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
}
impl IndexReadView<'_, '_> {
    fn native(&self) -> Result<&Native, ports::Error> {
        self.native.as_ref().ok_or(ports::Error::WriterStopped)
    }
    fn read_snapshot<T>(
        &mut self,
        read: impl FnOnce(&Native) -> Result<T, ports::Error>,
    ) -> Result<T, ports::Error> {
        self.native()?.read_snapshot(read)
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
            let returned = if !matches!(lock(&native.snapshot_failure), Ok(failure) if failure.is_none())
            {
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
fn validate_recipients(
    view: &mut TransactionView<'_>,
    operations: Operations<'_, '_>,
    value: &mut [u8],
) -> Result<(), ports::Error> {
    for position in 0..operations.len() {
        view.native.check()?;
        let Some(id) = operations.submission(position)? else {
            continue;
        };
        // At most 4096 supplied operations; no queue-sized set or account scan.
        let mut seen = false;
        for prior in 0..position {
            if operations.submission(prior)? == Some(id) {
                seen = true;
                break;
            }
        }
        if seen {
            continue;
        }
        let Some((row, _)) = view.get(Key::Submission(id), value)? else {
            // Deferred foreign keys reject any children of a deleted parent.
            continue;
        };
        let Row::Submission(row) = row else {
            return Err(ports::Error::Corrupt);
        };
        let count = row.recipient_count;
        let mut group = crate::recipient_sweep::QueueGroup::new(row);
        for ordinal in 0..count {
            let Some((Row::Recipient(row), _)) = view.get(Key::Recipient(id, ordinal), value)?
            else {
                return Err(ports::Error::Conflict);
            };
            group.recipient(row).map_err(|_| ports::Error::Conflict)?;
        }
        let mut cursor = [0; 20];
        let last = count.checked_sub(1).ok_or(ports::Error::Corrupt)?;
        let length = Key::Recipient(id, last)
            .encode(&mut cursor)
            .map_err(|_| ports::Error::Corrupt)?;
        let after = cursor.get(..length).ok_or(ports::Error::Corrupt)?;
        let mut key = [0; 20];
        if view
            .next(Table::Recipients, Some(after), &mut key, value)?
            .is_some_and(|record| matches!(record.key, Key::Recipient(found, _) if found == id))
        {
            return Err(ports::Error::Conflict);
        }
        group.finish().map_err(|_| ports::Error::Conflict)?;
    }
    Ok(())
}

fn next_change(
    native: &Native,
    identity: ViewIdentity,
    after: ChangeCursor,
    kind: ObjectType,
) -> Result<ChangeStep, ports::Error> {
    match kind {
        ObjectType::Mailbox
        | ObjectType::Thread
        | ObjectType::Email
        | ObjectType::EmailSubmission => {}
        ObjectType::Identity => {
            native.check()?;
            return Err(ports::Error::Invalid);
        }
    }
    if after.sequence < identity.history_floor
        || (after.sequence == identity.history_floor
            && identity.history_floor != Sequence::default()
            && after.operation != u32::MAX)
    {
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
        let seq = fixed_blob::<8>(row, 0)?;
        let operation: u32 = row.get(1).map_err(sql)?;
        if u64::from(operation) >= MAX_OPERATIONS as u64 {
            return Err(ports::Error::Corrupt);
        }
        let action: i64 = row.get(2).map_err(sql)?;
        let action = match action {
            1 => ChangeAction::Created,
            2 => ChangeAction::Updated,
            3 => ChangeAction::Destroyed,
            _ => return Err(ports::Error::Corrupt),
        };
        let id = fixed_blob::<16>(row, 3)?;
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
    fn account_enumeration_is_complete_capped_and_fenced() {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let clock = Arc::new(Timer(AtomicU64::new(1)));
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            clock.clone(),
            1,
            deadline(),
        )
        .unwrap();
        assert_eq!(store.account_ids(deadline()).unwrap(), Vec::new());
        for ordinal in (0..MAX_ACCOUNTS).rev() {
            store
                .create_account(
                    AccountId::from_bytes(u128::from(ordinal).to_be_bytes()),
                    deadline(),
                )
                .unwrap();
        }
        let expected: Vec<_> = (0..MAX_ACCOUNTS)
            .map(|ordinal| AccountId::from_bytes(u128::from(ordinal).to_be_bytes()))
            .collect();
        assert_eq!(store.account_ids(deadline()).unwrap(), expected);
        let writer = store.writer.lock().unwrap();
        assert_eq!(store.account_ids(deadline()), Err(ports::Error::Busy));
        drop(writer);
        clock.0.store(101, Ordering::Relaxed);
        assert_eq!(store.account_ids(deadline()), Err(ports::Error::Deadline));
        clock.0.store(1, Ordering::Relaxed);
        {
            let mut writer = store.writer.lock().unwrap();
            writer.stopped = true;
        }
        assert_eq!(
            store.account_ids(deadline()),
            Err(ports::Error::WriterStopped)
        );
        {
            let mut writer = store.writer.lock().unwrap();
            writer.stopped = false;
            writer.native.begin_work(deadline()).unwrap();
            writer
                .native
                .run(|db| {
                    db.execute(
                        "INSERT INTO accounts VALUES(?1,?2,?2)",
                        params![
                            u128::from(MAX_ACCOUNTS).to_be_bytes().as_slice(),
                            0u64.to_be_bytes().as_slice()
                        ],
                    )
                    .map_err(sql)?;
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(store.account_ids(deadline()), Err(ports::Error::Capacity));
    }

    #[test]
    fn writer_entry_observation_is_preserved_by_native_scope_handoff() {
        const COUNTS: &str =
            "SELECT (SELECT count(*) FROM accounts),(SELECT count(*) FROM changes)";
        struct HandoffClock(Mutex<(std::collections::VecDeque<ports::Tick>, ports::Tick)>);
        impl Clock for HandoffClock {
            fn sample(&self) -> Result<ports::Time, ports::Error> {
                let mut state = self.0.lock().unwrap();
                if let Some(next) = state.0.pop_front() {
                    state.1 = next;
                }
                Ok(ports::Time {
                    utc_ms: 0,
                    monotonic: state.1,
                })
            }
        }
        fn rejected(result: Result<(), CommitError>) -> Result<(), ports::Error> {
            result.map_err(|error| match error {
                CommitError::Rejected(error) => error,
                CommitError::Indeterminate(error) => {
                    panic!("unexpected uncertain commit: {error:?}")
                }
            })
        }
        let mut wrong = Vec::new();
        for entry in [
            "create_account",
            "commit",
            "prune_history",
            "view",
            "maintenance_view",
            "validate_integrity",
            "usage_fence",
            "checkpoint",
        ] {
            for (acquired, started) in [(9, 8), (8, 8), (8, 9)] {
                let fixture = Fixture::new();
                let mut root = fixture.locked();
                let clock = Arc::new(HandoffClock(Mutex::new((
                    std::collections::VecDeque::new(),
                    Tick(1),
                ))));
                let store = IndexStore::create(
                    &mut root,
                    StoreEpoch::from_bytes([9; 16]),
                    clock.clone(),
                    1,
                    deadline(),
                )
                .unwrap();
                store.create_account(ACCOUNT, deadline()).unwrap();
                let initial = mailbox("initial", None);
                let initial_ops = [
                    Operation::put(Table::Mailboxes, ID.as_bytes(), &initial).unwrap(),
                    Operation::change(ObjectType::Mailbox, ChangeAction::Created, ID.as_bytes()),
                ];
                let request = CommitRequest {
                    account: ACCOUNT,
                    epoch: StoreEpoch::from_bytes([9; 16]),
                    expected: Sequence::default(),
                    utc_ms: 0,
                    deadline: deadline(),
                };
                store
                    .commit(&td_crypto::Provider, request, &initial_ops, &mut [])
                    .unwrap();
                let identity = store.view(ACCOUNT, deadline()).unwrap().identity();
                let changed = mailbox("changed", None);
                let changed_ops = [
                    Operation::put(Table::Mailboxes, ID.as_bytes(), &changed).unwrap(),
                    Operation::change(ObjectType::Mailbox, ChangeAction::Updated, ID.as_bytes()),
                ];
                clock.0.lock().unwrap().0 = [Tick(acquired), Tick(started)].into();
                let actual = match entry {
                    "create_account" => {
                        rejected(store.create_account(AccountId::from_bytes([3; 16]), deadline()))
                    }
                    "commit" => rejected(
                        store
                            .commit(
                                &td_crypto::Provider,
                                CommitRequest {
                                    expected: Sequence::from_u64(1),
                                    ..request
                                },
                                &changed_ops,
                                &mut [],
                            )
                            .map(|_| ()),
                    ),
                    "prune_history" => rejected(
                        store
                            .prune_history(HistoryPruneRequest {
                                account: ACCOUNT,
                                expected: Sequence::from_u64(1),
                                through: Sequence::from_u64(1),
                                max_rows: 1,
                                deadline: deadline(),
                            })
                            .map(|_| ()),
                    ),
                    "view" => store.view(ACCOUNT, deadline()).map(drop),
                    "maintenance_view" => store.maintenance_view(ACCOUNT, deadline()).map(drop),
                    "validate_integrity" => store.validate_integrity(deadline()),
                    "usage_fence" => store.usage_fence(deadline()).map(drop),
                    "checkpoint" => store.checkpoint(deadline()),
                    _ => panic!("unknown fixture"),
                };
                assert!(clock.0.lock().unwrap().0.is_empty(), "{entry}");
                if started < acquired {
                    if actual != Err(ports::Error::Invalid) {
                        wrong.push((entry, "handoff", actual));
                    } else {
                        let writer = lock(&store.writer).unwrap();
                        assert!(
                            lock(&writer.native.connection).unwrap().is_autocommit(),
                            "{entry}"
                        );
                        assert!(!writer.stopped, "{entry}");
                        let sticky = if matches!(entry, "view" | "maintenance_view") {
                            let readers = lock(&store.readers).unwrap();
                            let ReaderSlot::Available(native) = readers.first().unwrap() else {
                                panic!("refused capture did not return its slot")
                            };
                            native.check()
                        } else {
                            writer.native.check()
                        };
                        if sticky != Err(ports::Error::Invalid) {
                            wrong.push((entry, "stickiness", sticky));
                        }
                        drop(writer);
                        let mut next = store.view(ACCOUNT, deadline()).unwrap();
                        assert_eq!(next.identity(), identity, "{entry}");
                        let mut bytes = [0; 128];
                        let (row, sequence) =
                            next.get(Key::Mailbox(ID), &mut bytes).unwrap().unwrap();
                        assert!(
                            matches!(row, Row::Mailbox(row) if row.name == "initial"),
                            "{entry}"
                        );
                        assert_eq!(sequence, Sequence::from_u64(1), "{entry}");
                        let counts: (i64, i64) = next
                            .native()
                            .unwrap()
                            .run(|db| {
                                db.query_row(COUNTS, [], |row| Ok((row.get(0)?, row.get(1)?)))
                                    .map_err(sql)
                            })
                            .unwrap();
                        assert_eq!(counts, (1, 1), "{entry}");
                        drop(next);
                        if !matches!(entry, "view" | "maintenance_view") {
                            assert_eq!(store.validate_integrity(deadline()), Ok(()), "{entry}");
                        }
                    }
                } else {
                    assert_eq!(actual, Ok(()), "{entry} {acquired}->{started}");
                }
            }
        }
        assert!(wrong.is_empty(), "reversed handoffs accepted: {wrong:?}");
    }

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
                    epoch: StoreEpoch::from_bytes([9; 16]),
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
                    epoch: StoreEpoch::from_bytes([9; 16]),
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
    fn body_owners_refuse_lost_snapshots_and_retire_the_pool_slot() {
        use super::super::pinned::snapshot_tests::{input_native, pin_native};
        let mut wrong = Vec::new();
        for stage in ["input_read", "input_finish", "pin_read", "pin_check"] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let clock = Arc::new(Timer(AtomicU64::new(1)));
            let store = IndexStore::create(
                &mut root,
                StoreEpoch::from_bytes([9; 16]),
                clock.clone(),
                1,
                deadline(),
            )
            .unwrap();
            store.create_account(ACCOUNT, deadline()).unwrap();
            let id = BlobId::from_bytes([4; 16]);
            let replacement = BlobId::from_bytes([5; 16]);
            let value = encode(Row::Blob(body_row(b"original", BlobKind::Message)));
            store
                .commit(
                    &td_crypto::Provider,
                    request(0),
                    &[Operation::put(Table::Blobs, id.as_bytes(), &value).unwrap()],
                    &mut [BlobSource {
                        id,
                        source: &mut b"original".as_slice(),
                    }],
                )
                .unwrap();
            let mut view = store.view(ACCOUNT, deadline()).unwrap();
            let mut input = view
                .open_blob_input(&td_crypto::Provider, id, MAX_BODY_BYTES)
                .unwrap();
            let native = input_native(&input);
            let selected = Arc::new(AtomicU64::new(0));
            let observed = selected.clone();
            lock(&native.connection)
                .unwrap()
                .authorizer(Some(move |context: AuthContext<'_>| {
                    if matches!(context.action, AuthAction::Select) {
                        observed.fetch_add(1, Ordering::Relaxed);
                    }
                    Authorization::Allow
                }))
                .unwrap();
            assert_eq!(input.read(&mut []), Ok(0));
            assert!(selected.load(Ordering::Relaxed) > 0);
            let mut output = [0; 8];
            if stage != "input_read" {
                assert_eq!(input.read(&mut output), Ok(8));
                assert_eq!(&output, b"original");
            }
            let (mut input, mut pin) = if stage.starts_with("pin_") {
                (None, Some(input.finish().unwrap()))
            } else {
                (Some(input), None)
            };
            let native = pin.as_ref().map(pin_native).unwrap_or(native);
            native
                .run(|db| db.execute_batch("ROLLBACK").map_err(sql))
                .unwrap();
            let changed = encode(Row::Blob(body_row(b"replaced", BlobKind::Message)));
            store
                .commit(
                    &td_crypto::Provider,
                    request(1),
                    &[
                        Operation::delete(Table::Blobs, id.as_bytes()).unwrap(),
                        Operation::put(Table::Blobs, replacement.as_bytes(), &changed).unwrap(),
                    ],
                    &mut [BlobSource {
                        id: replacement,
                        source: &mut b"replaced".as_slice(),
                    }],
                )
                .unwrap();
            selected.store(0, Ordering::Relaxed);
            let actual = match stage {
                "input_read" => input.as_mut().unwrap().read(&mut output).map(|_| ()),
                "input_finish" => input.take().unwrap().finish().map(drop),
                "pin_read" => {
                    ports::BlobReader::read_at(pin.as_mut().unwrap(), 0, &mut output).map(|_| ())
                }
                "pin_check" => pin.as_mut().unwrap().check_deadline(),
                _ => panic!("unknown stage"),
            };
            if actual != Err(ports::Error::Corrupt) {
                wrong.push((stage, actual, output));
            }
            if actual == Err(ports::Error::Corrupt) {
                assert_eq!(selected.load(Ordering::Relaxed), 0, "{stage}");
                native
                    .run(|db| db.execute_batch("BEGIN DEFERRED").map_err(sql))
                    .unwrap();
                if let Some(input) = input.as_mut() {
                    assert_eq!(input.read(&mut output), Err(ports::Error::Corrupt));
                }
                if let Some(pin) = pin.as_mut() {
                    assert_eq!(pin.check_deadline(), Err(ports::Error::Corrupt));
                    assert_eq!(
                        ports::BlobReader::read_at(pin, 0, &mut output),
                        Err(ports::Error::Corrupt)
                    );
                }
            }
            drop(input);
            drop(pin);
            let result = view.get(Key::Blob(replacement), &mut [0; 64]).map(|_| ());
            assert_eq!(result, Err(ports::Error::Corrupt));
            drop(view);
            assert!(matches!(
                store.view(ACCOUNT, deadline()),
                Err(ports::Error::Busy)
            ));
            drop(store);
            let reopened = IndexStore::open(&mut root, clock, 1, deadline()).unwrap();
            let mut fresh = reopened.view(ACCOUNT, deadline()).unwrap();
            let mut healthy = fresh
                .open_blob_input(&td_crypto::Provider, replacement, MAX_BODY_BYTES)
                .unwrap();
            assert_eq!(healthy.read(&mut output), Ok(8));
            assert_eq!(&output, b"replaced");
            assert!(healthy.finish().is_ok());
        }
        assert!(wrong.is_empty(), "lost body snapshots accepted: {wrong:?}");
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
                    epoch: StoreEpoch::from_bytes([9; 16]),
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
                    epoch: StoreEpoch::from_bytes([9; 16]),
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
                    epoch: StoreEpoch::from_bytes([9; 16]),
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
                    epoch: StoreEpoch::from_bytes([9; 16]),
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
                    epoch: StoreEpoch::from_bytes([9; 16]),
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
                    epoch: StoreEpoch::from_bytes([9; 16]),
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
                    epoch: StoreEpoch::from_bytes([9; 16]),
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
                    epoch: StoreEpoch::from_bytes([9; 16]),
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
    fn account_identity_rejects_nonexact_blob_widths() {
        let mut wrong = Vec::new();
        for column in ["sequence", "floor"] {
            for length in [0, 7, 9] {
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
                let old = store.view(ACCOUNT, deadline()).unwrap();
                let original = old.identity();
                lock(&store.writer)
                    .unwrap()
                    .native
                    .run(|db| {
                        db.execute_batch("PRAGMA ignore_check_constraints=ON")
                            .map_err(sql)?;
                        let statement = if column == "sequence" {
                            "UPDATE accounts SET sequence=?1 WHERE id=?2"
                        } else {
                            "UPDATE accounts SET floor=?1 WHERE id=?2"
                        };
                        assert_eq!(
                            db.execute(
                                statement,
                                params![vec![0u8; length], ACCOUNT.as_bytes().as_slice()]
                            )
                            .map_err(sql)?,
                            1
                        );
                        db.execute_batch("PRAGMA ignore_check_constraints=OFF")
                            .map_err(sql)
                    })
                    .unwrap();
                let actual = store.view(ACCOUNT, deadline()).map(|view| view.identity());
                if actual != Err(ports::Error::Corrupt) {
                    wrong.push((column, length, actual));
                }
                assert_eq!(
                    old.native()
                        .unwrap()
                        .run(|db| identity(db, ACCOUNT, original.epoch)),
                    Ok(original)
                );
                lock(&store.writer)
                    .unwrap()
                    .native
                    .run(|db| {
                        db.execute(
                            "UPDATE accounts SET sequence=?1,floor=?1 WHERE id=?2",
                            params![0u64.to_be_bytes().as_slice(), ACCOUNT.as_bytes().as_slice()],
                        )
                        .map_err(sql)?;
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(
                    store.view(ACCOUNT, deadline()).unwrap().identity(),
                    original
                );
            }
        }
        assert!(wrong.is_empty(), "nonexact account metadata: {wrong:?}");
    }
    #[test]
    fn change_reader_rejects_nonexact_blob_widths() {
        let mut wrong = Vec::new();
        for (column, length) in [
            ("sequence", 7),
            ("sequence", 9),
            ("object", 0),
            ("object", 15),
            ("object", 17),
        ] {
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
            let operations = [
                Operation::put(Table::Mailboxes, ID.as_bytes(), &value).unwrap(),
                Operation::change(ObjectType::Mailbox, ChangeAction::Created, ID.as_bytes()),
            ];
            assert_eq!(
                store.commit(
                    &td_crypto::Provider,
                    CommitRequest {
                        account: ACCOUNT,
                        epoch: StoreEpoch::from_bytes([9; 16]),
                        expected: Sequence::default(),
                        utc_ms: 0,
                        deadline: deadline(),
                    },
                    &operations,
                    &mut []
                ),
                Ok(Sequence::from_u64(1))
            );
            // Keep both malformed sequence blobs inside the native range seek.
            lock(&store.writer)
                .unwrap()
                .native
                .run(|db| {
                    db.execute(
                        "UPDATE accounts SET sequence=?1 WHERE id=?2",
                        params![
                            512u64.to_be_bytes().as_slice(),
                            ACCOUNT.as_bytes().as_slice()
                        ],
                    )
                    .map_err(sql)?;
                    Ok(())
                })
                .unwrap();
            let start = ChangeCursor {
                sequence: Sequence::default(),
                operation: u32::MAX,
            };
            let expected = ChangeStep::Record(ChangeRecord {
                cursor: ChangeCursor {
                    sequence: Sequence::from_u64(1),
                    operation: 1,
                },
                change: Change {
                    kind: ObjectType::Mailbox,
                    id: *ID.as_bytes(),
                    action: ChangeAction::Created,
                },
            });
            let mut old = store.view(ACCOUNT, deadline()).unwrap();
            assert_eq!(old.next_change(start, ObjectType::Mailbox), Ok(expected));
            let mut malformed = vec![0u8; length];
            if column == "sequence" {
                if length == 7 {
                    malformed[6] = 1;
                } else {
                    malformed[7] = 1;
                }
            }
            lock(&store.writer)
                .unwrap()
                .native
                .run(|db| {
                    db.execute_batch("PRAGMA ignore_check_constraints=ON")
                        .map_err(sql)?;
                    let statement = if column == "sequence" {
                        "UPDATE changes SET sequence=?1 WHERE account=?2"
                    } else {
                        "UPDATE changes SET object=?1 WHERE account=?2"
                    };
                    assert_eq!(
                        db.execute(statement, params![malformed, ACCOUNT.as_bytes().as_slice()])
                            .map_err(sql)?,
                        1
                    );
                    db.execute_batch("PRAGMA ignore_check_constraints=OFF")
                        .map_err(sql)
                })
                .unwrap();
            let mut current = store.view(ACCOUNT, deadline()).unwrap();
            let actual = current.next_change(start, ObjectType::Mailbox);
            if actual != Err(ports::Error::Corrupt) {
                wrong.push((column, length, actual));
            }
            assert_eq!(old.next_change(start, ObjectType::Mailbox), Ok(expected));
        }
        assert!(wrong.is_empty(), "nonexact change metadata: {wrong:?}");
    }
    #[test]
    fn change_reader_rejects_out_of_range_stored_operation_ordinals() {
        let mut wrong = Vec::new();
        for ordinal in [
            0,
            4095,
            -1,
            4096,
            i64::from(u32::MAX),
            i64::from(u32::MAX) + 1,
        ] {
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
            let operations = [
                Operation::put(Table::Mailboxes, ID.as_bytes(), &value).unwrap(),
                Operation::change(ObjectType::Mailbox, ChangeAction::Created, ID.as_bytes()),
            ];
            assert_eq!(
                store.commit(
                    &td_crypto::Provider,
                    CommitRequest {
                        account: ACCOUNT,
                        epoch: StoreEpoch::from_bytes([9; 16]),
                        expected: Sequence::default(),
                        utc_ms: 0,
                        deadline: deadline(),
                    },
                    &operations,
                    &mut []
                ),
                Ok(Sequence::from_u64(1))
            );
            let start = ChangeCursor {
                sequence: Sequence::default(),
                operation: u32::MAX,
            };
            let mut old = store.view(ACCOUNT, deadline()).unwrap();
            let original = old.next_change(start, ObjectType::Mailbox).unwrap();
            assert_eq!(
                original,
                ChangeStep::Record(ChangeRecord {
                    cursor: ChangeCursor {
                        sequence: Sequence::from_u64(1),
                        operation: 1
                    },
                    change: Change {
                        kind: ObjectType::Mailbox,
                        id: *ID.as_bytes(),
                        action: ChangeAction::Created
                    },
                })
            );
            lock(&store.writer)
                .unwrap()
                .native
                .run(|db| {
                    db.execute_batch("PRAGMA ignore_check_constraints=ON")
                        .map_err(sql)?;
                    assert_eq!(
                        db.execute(
                            "UPDATE changes SET operation=?1 WHERE account=?2",
                            params![ordinal, ACCOUNT.as_bytes().as_slice()]
                        )
                        .map_err(sql)?,
                        1
                    );
                    db.execute_batch("PRAGMA ignore_check_constraints=OFF")
                        .map_err(sql)
                })
                .unwrap();
            let mut current = store.view(ACCOUNT, deadline()).unwrap();
            let actual = current.next_change(start, ObjectType::Mailbox);
            if (0..=4095).contains(&ordinal) {
                let expected = ChangeRecord {
                    cursor: ChangeCursor {
                        sequence: Sequence::from_u64(1),
                        operation: u32::try_from(ordinal).unwrap(),
                    },
                    change: Change {
                        kind: ObjectType::Mailbox,
                        id: *ID.as_bytes(),
                        action: ChangeAction::Created,
                    },
                };
                assert_eq!(actual, Ok(ChangeStep::Record(expected)));
                assert_eq!(
                    current.next_change(expected.cursor, ObjectType::Mailbox),
                    Ok(ChangeStep::Complete)
                );
                assert_eq!(
                    current.next_change(
                        ChangeCursor {
                            sequence: Sequence::from_u64(1),
                            operation: u32::MAX
                        },
                        ObjectType::Mailbox
                    ),
                    Ok(ChangeStep::Complete)
                );
            } else if actual != Err(ports::Error::Corrupt) {
                wrong.push((ordinal, actual));
            }
            assert_eq!(old.next_change(start, ObjectType::Mailbox), Ok(original));
        }
        assert!(wrong.is_empty(), "out-of-range stored ordinals: {wrong:?}");
    }
    #[test]
    fn account_snapshot_floor_cannot_exceed_its_endpoint() {
        let mut wrong_views = Vec::new();
        let mut wrong_commits = Vec::new();
        for (endpoint, floor) in [
            (0, 0),
            (7, 0),
            (7, 7),
            (u64::MAX, 1 << 63),
            (u64::MAX, u64::MAX),
            (0, 1),
            (7, 8),
            (u64::MAX - 1, u64::MAX),
            (7, 1 << 63),
        ] {
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
            let old = store.view(ACCOUNT, deadline()).unwrap();
            let original = old.identity();
            lock(&store.writer)
                .unwrap()
                .native
                .run(|db| {
                    assert_eq!(
                        db.execute(
                            "UPDATE accounts SET sequence=?1,floor=?2 WHERE id=?3",
                            params![
                                endpoint.to_be_bytes().as_slice(),
                                floor.to_be_bytes().as_slice(),
                                ACCOUNT.as_bytes().as_slice()
                            ]
                        )
                        .map_err(sql)?,
                        1
                    );
                    Ok(())
                })
                .unwrap();
            let actual = store.view(ACCOUNT, deadline()).map(|view| view.identity());
            if floor <= endpoint {
                assert_eq!(
                    actual,
                    Ok(ViewIdentity {
                        committed_sequence: Sequence::from_u64(endpoint),
                        history_floor: Sequence::from_u64(floor),
                        ..original
                    })
                );
            } else if actual != Err(ports::Error::Corrupt) {
                wrong_views.push((endpoint, floor, actual));
            }
            let value = mailbox("one", None);
            let put = Operation::put(Table::Mailboxes, ID.as_bytes(), &value).unwrap();
            let committed = store.commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    epoch: StoreEpoch::from_bytes([9; 16]),
                    expected: Sequence::from_u64(endpoint),
                    utc_ms: 0,
                    deadline: deadline(),
                },
                &[put],
                &mut [],
            );
            if floor <= endpoint {
                let expected = endpoint
                    .checked_add(1)
                    .map(Sequence::from_u64)
                    .ok_or(CommitError::Rejected(ports::Error::Capacity));
                assert_eq!(committed, expected);
            } else if committed != Err(CommitError::Rejected(ports::Error::Corrupt)) {
                wrong_commits.push((endpoint, floor, committed));
            } else {
                lock(&store.writer)
                    .unwrap()
                    .native
                    .run(|db| {
                        let actual: (Vec<u8>, Vec<u8>) = db
                            .query_row(
                                "SELECT sequence,floor FROM accounts WHERE id=?1",
                                [ACCOUNT.as_bytes().as_slice()],
                                |row| Ok((row.get(0)?, row.get(1)?)),
                            )
                            .map_err(sql)?;
                        assert_eq!(
                            actual,
                            (
                                endpoint.to_be_bytes().to_vec(),
                                floor.to_be_bytes().to_vec()
                            )
                        );
                        assert_eq!(
                            db.query_row(
                                "SELECT count(*) FROM mailboxes WHERE account=?1",
                                [ACCOUNT.as_bytes().as_slice()],
                                |row| row.get::<_, u32>(0)
                            )
                            .map_err(sql)?,
                            0
                        );
                        Ok(())
                    })
                    .unwrap();
            }
            assert_eq!(
                old.native()
                    .unwrap()
                    .run(|db| identity(db, ACCOUNT, original.epoch)),
                Ok(original)
            );
        }
        assert!(
            wrong_views.is_empty() && wrong_commits.is_empty(),
            "incoherent views: {wrong_views:?}; commits: {wrong_commits:?}"
        );
    }
    #[test]
    fn native_change_reader_refuses_identity_requests_before_querying() {
        const INSERT_IDENTITY: &str = "INSERT INTO changes(account,sequence,operation,kind,action,object) VALUES(?1,?2,2,4,1,?3)";
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
        let start = ChangeCursor {
            sequence: Sequence::default(),
            operation: u32::MAX,
        };
        let mut wrong = Vec::new();
        fn identity_request(
            view: &mut IndexReadView<'_, '_>,
            start: ChangeCursor,
            wrong: &mut Vec<Result<ChangeStep, ports::Error>>,
        ) {
            let seen = Arc::new(AtomicU64::new(0));
            let reads = Arc::clone(&seen);
            lock(&view.native().unwrap().connection)
                .unwrap()
                .authorizer(Some(move |context: AuthContext<'_>| {
                    if matches!(context.action, AuthAction::Select) {
                        reads.fetch_add(1, Ordering::Relaxed);
                    }
                    Authorization::Allow
                }))
                .unwrap();
            let actual = view.next_change(start, ObjectType::Identity);
            if actual != Err(ports::Error::Invalid) {
                wrong.push(actual);
            } else {
                assert_eq!(seen.load(Ordering::Relaxed), 0);
            }
            assert!(view.next_change(start, ObjectType::Mailbox).is_ok());
            assert!(seen.load(Ordering::Relaxed) > 0);
            lock(&view.native().unwrap().connection)
                .unwrap()
                .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .unwrap();
        }
        let mut old = store.view(ACCOUNT, deadline()).unwrap();
        identity_request(&mut old, start, &mut wrong);
        for kind in [
            ObjectType::Mailbox,
            ObjectType::Thread,
            ObjectType::Email,
            ObjectType::EmailSubmission,
        ] {
            assert_eq!(old.next_change(start, kind), Ok(ChangeStep::Complete));
        }
        let request = CommitRequest {
            account: ACCOUNT,
            epoch: StoreEpoch::from_bytes([9; 16]),
            expected: Sequence::default(),
            utc_ms: 0,
            deadline: deadline(),
        };
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                request,
                &[Operation::change(
                    ObjectType::Identity,
                    ChangeAction::Created,
                    ID.as_bytes()
                )],
                &mut []
            ),
            Err(CommitError::Rejected(ports::Error::Invalid))
        );
        let value = mailbox("one", None);
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                request,
                &[
                    Operation::put(Table::Mailboxes, ID.as_bytes(), &value).unwrap(),
                    Operation::change(ObjectType::Mailbox, ChangeAction::Created, ID.as_bytes()),
                ],
                &mut []
            ),
            Ok(Sequence::from_u64(1))
        );
        lock(&store.writer)
            .unwrap()
            .native
            .run(|db| {
                db.execute_batch("PRAGMA ignore_check_constraints=ON")
                    .map_err(sql)?;
                assert_eq!(
                    db.execute(
                        INSERT_IDENTITY,
                        params![
                            ACCOUNT.as_bytes().as_slice(),
                            1u64.to_be_bytes().as_slice(),
                            ID.as_bytes().as_slice()
                        ]
                    )
                    .map_err(sql)?,
                    1
                );
                db.execute_batch("PRAGMA ignore_check_constraints=OFF")
                    .map_err(sql)
            })
            .unwrap();
        let mut current = store.view(ACCOUNT, deadline()).unwrap();
        identity_request(&mut current, start, &mut wrong);
        let expected = ChangeRecord {
            cursor: ChangeCursor {
                sequence: Sequence::from_u64(1),
                operation: 1,
            },
            change: Change {
                kind: ObjectType::Mailbox,
                id: *ID.as_bytes(),
                action: ChangeAction::Created,
            },
        };
        assert_eq!(
            current.next_change(start, ObjectType::Mailbox),
            Ok(ChangeStep::Record(expected))
        );
        assert_eq!(
            current.next_change(expected.cursor, ObjectType::Mailbox),
            Ok(ChangeStep::Complete)
        );
        assert_eq!(
            old.next_change(start, ObjectType::Mailbox),
            Ok(ChangeStep::Complete)
        );
        clock.0.store(101, Ordering::Relaxed);
        assert_eq!(
            current.next_change(start, ObjectType::Identity),
            Err(ports::Error::Deadline)
        );
        clock.0.store(1, Ordering::Relaxed);
        assert_eq!(
            current.next_change(start, ObjectType::Identity),
            Err(ports::Error::Deadline)
        );
        assert!(wrong.is_empty(), "unsupported Identity requests: {wrong:?}");
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
            epoch: StoreEpoch::from_bytes([9; 16]),
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
            epoch: StoreEpoch::from_bytes([9; 16]),
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
            epoch: StoreEpoch::from_bytes([9; 16]),
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
    fn full_reader_pool_preserves_bodies_across_committed_deletion() {
        use crate::ports::BlobReader;
        const READERS: usize = 8;
        const CHUNK: usize = 64 * 1024;
        const LENGTH: usize = 2 * 1024 * 1024;
        for completed_before_delete in [false, true] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let clock = Arc::new(Timer(AtomicU64::new(1)));
            let epoch = StoreEpoch::from_bytes([0x76; 16]);
            let store =
                IndexStore::create(&mut root, epoch, clock.clone(), READERS, deadline()).unwrap();
            store.create_account(ACCOUNT, deadline()).unwrap();
            let id = BlobId::from_bytes([0x77; 16]);
            let raw: Vec<u8> = (0..LENGTH)
                .map(|n| ((n / CHUNK) ^ (n % 251)) as u8)
                .collect();
            let row = body_row(&raw, BlobKind::Message);
            let value = encode(Row::Blob(row));
            let put = Operation::put(Table::Blobs, id.as_bytes(), &value).unwrap();
            assert_eq!(
                store.commit(
                    &td_crypto::Provider,
                    CommitRequest {
                        epoch,
                        ..request(0)
                    },
                    &[put],
                    &mut [BlobSource {
                        id,
                        source: &mut raw.as_slice(),
                    }]
                ),
                Ok(Sequence::from_u64(1))
            );
            store.checkpoint(deadline()).unwrap();
            let old = ViewIdentity {
                account: ACCOUNT,
                epoch,
                committed_sequence: Sequence::from_u64(1),
                history_floor: Sequence::default(),
            };
            let current = ViewIdentity {
                committed_sequence: Sequence::from_u64(2),
                ..old
            };
            let mut views: [_; READERS] =
                std::array::from_fn(|_| Some(store.view(ACCOUNT, deadline()).unwrap()));
            assert!(views
                .iter()
                .all(|view| view.as_ref().unwrap().identity() == old));
            assert!(matches!(
                store.view(ACCOUNT, deadline()),
                Err(ports::Error::Busy)
            ));
            let mut scratch = vec![0; CHUNK];
            let remove = || {
                assert_eq!(
                    store.commit(
                        &td_crypto::Provider,
                        CommitRequest {
                            epoch,
                            ..request(1)
                        },
                        &[Operation::delete(Table::Blobs, id.as_bytes()).unwrap()],
                        &mut []
                    ),
                    Ok(Sequence::from_u64(2))
                );
                assert!(matches!(
                    store.view(ACCOUNT, deadline()),
                    Err(ports::Error::Busy)
                ));
            };
            {
                let mut inputs = views.each_mut().map(|view| {
                    view.as_mut()
                        .unwrap()
                        .open_blob_input(&td_crypto::Provider, id, LENGTH as u64)
                        .unwrap()
                });
                for input in &mut inputs {
                    assert_eq!(input.read(&mut scratch).unwrap(), CHUNK);
                    assert_eq!(scratch, raw[..CHUNK]);
                }
                if !completed_before_delete {
                    remove();
                }
                for position in (CHUNK..LENGTH).step_by(CHUNK) {
                    for input in &mut inputs {
                        assert_eq!(input.position(), position as u64);
                        assert_eq!(input.read(&mut scratch).unwrap(), CHUNK);
                        assert_eq!(scratch, raw[position..position + CHUNK]);
                    }
                }
                assert!(
                    inputs
                        .iter()
                        .all(|input| input.position() == LENGTH as u64
                            && input.len() == LENGTH as u64)
                );
                let mut pins = inputs.map(|input| input.finish().unwrap());
                if completed_before_delete {
                    remove();
                }
                for pin in &mut pins {
                    assert_eq!(pin.len(), LENGTH as u64);
                    for position in (0..LENGTH).step_by(CHUNK) {
                        assert_eq!(pin.read_at(position as u64, &mut scratch).unwrap(), CHUNK);
                        assert_eq!(scratch, raw[position..position + CHUNK]);
                    }
                    assert_eq!(
                        pin.read_at((CHUNK - 1) as u64, &mut scratch).unwrap(),
                        CHUNK
                    );
                    assert_eq!(scratch, raw[CHUNK - 1..2 * CHUNK - 1]);
                    assert_eq!(pin.read_at((LENGTH - 1) as u64, &mut scratch).unwrap(), 1);
                    assert_eq!(scratch[0], raw[LENGTH - 1]);
                    assert_eq!(pin.read_at(LENGTH as u64, &mut scratch).unwrap(), 0);
                }
                assert_eq!(store.checkpoint(deadline()), Err(ports::Error::Busy));
            }
            let mut metadata = [0; 128];
            for view in views.iter_mut().flatten() {
                assert_eq!(view.identity(), old);
                assert_eq!(
                    view.get(Key::Blob(id), &mut metadata).unwrap(),
                    Some((Row::Blob(row), Sequence::from_u64(1)))
                );
            }
            drop(views.first_mut().unwrap().take());
            let mut fresh = store.view(ACCOUNT, deadline()).unwrap();
            assert_eq!(fresh.identity(), current);
            assert!(fresh.get(Key::Blob(id), &mut metadata).unwrap().is_none());
            assert!(matches!(
                fresh.open_blob_input(&td_crypto::Provider, id, LENGTH as u64),
                Err(ports::Error::NotFound)
            ));
            assert!(matches!(
                store.view(ACCOUNT, deadline()),
                Err(ports::Error::Busy)
            ));
            assert_eq!(
                store.commit(
                    &td_crypto::Provider,
                    CommitRequest {
                        epoch,
                        ..request(2)
                    },
                    &[put],
                    &mut [BlobSource {
                        id,
                        source: &mut raw.as_slice(),
                    }]
                ),
                Err(CommitError::Rejected(ports::Error::Conflict))
            );
            for view in views.iter_mut().flatten() {
                assert_eq!(view.identity(), old);
                assert_eq!(
                    view.get(Key::Blob(id), &mut metadata).unwrap(),
                    Some((Row::Blob(row), Sequence::from_u64(1)))
                );
            }
            assert_eq!(store.checkpoint(deadline()), Err(ports::Error::Busy));
            drop(fresh);
            drop(views);
            let returned: [_; READERS] =
                std::array::from_fn(|_| store.view(ACCOUNT, deadline()).unwrap());
            assert!(returned.iter().all(|view| view.identity() == current));
            drop(returned);
            store.checkpoint(deadline()).unwrap();
            drop(store);
            let reopened = IndexStore::open(&mut root, clock, READERS, deadline()).unwrap();
            reopened.validate_integrity(deadline()).unwrap();
            let mut view = reopened.view(ACCOUNT, deadline()).unwrap();
            assert_eq!(view.identity(), current);
            assert!(view.get(Key::Blob(id), &mut metadata).unwrap().is_none());
            assert_eq!(
                reopened.commit(
                    &td_crypto::Provider,
                    CommitRequest {
                        epoch,
                        ..request(2)
                    },
                    &[put],
                    &mut [BlobSource {
                        id,
                        source: &mut raw.as_slice(),
                    }]
                ),
                Err(CommitError::Rejected(ports::Error::Conflict))
            );
            assert_eq!(view.identity(), current);
        }
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
    #[ignore = "explicit qualification writes over 4 GiB of WAL; ordinary gates stay bounded"]
    fn large_wal_checkpoint_fits_native_allocation_cap() {
        use std::{
            io::Read,
            time::{Duration, Instant},
        };
        const TARGET_FRAMES: u64 = 1_048_576;
        const MAX_ROUNDS: u64 = 160;
        const MAX_FIXTURE_WAL: u64 = 6 * 1024 * 1024 * 1024;
        let started = Instant::now();
        let timeout = Duration::from_secs(900);
        let fixture = Fixture::new();
        eprintln!("large-wal: root={}", fixture.path.display());
        let mut root = fixture.locked();
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            Arc::new(Timer(AtomicU64::new(1))),
            8,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        store.checkpoint(deadline()).unwrap();
        let mut snapshot = store.view(ACCOUNT, deadline()).unwrap();
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
        let wal = db_path(store.root(), RootEntry::Wal).unwrap();
        let mut previous: Option<BlobId> = None;
        let mut sequence = 0;
        let mut frames = 0;
        while frames < TARGET_FRAMES {
            assert!(
                started.elapsed() < timeout,
                "large WAL generation timed out"
            );
            assert!(
                sequence < MAX_ROUNDS,
                "large WAL fixture exceeded its write budget"
            );
            let mut identifier = [0x93; 16];
            identifier[8..].copy_from_slice(&sequence.to_be_bytes());
            let id = BlobId::from_bytes(identifier);
            let put = Operation::put(Table::Blobs, id.as_bytes(), &value).unwrap();
            let mut operations = vec![put];
            if let Some(previous) = previous.as_ref() {
                operations.insert(
                    0,
                    Operation::delete(Table::Blobs, previous.as_bytes()).unwrap(),
                );
            }
            let mut source = std::io::repeat(0x5a).take(MAX_BODY_BYTES);
            assert_eq!(
                store.commit(
                    &td_crypto::Provider,
                    request(sequence),
                    &operations,
                    &mut [BlobSource {
                        id,
                        source: &mut source
                    }],
                ),
                Ok(Sequence::from_u64(sequence + 1))
            );
            assert_eq!(source.limit(), 0);
            previous = Some(id);
            sequence += 1;
            let bytes = fs::metadata(&wal).unwrap().len();
            assert!(bytes <= MAX_FIXTURE_WAL);
            frames = (bytes - 32) / (PAGE_BYTES + 24);
            if sequence % 8 == 0 || frames >= TARGET_FRAMES {
                eprintln!(
                    "large-wal: commits={sequence} frames={frames} bytes={bytes} elapsed={:?}",
                    started.elapsed()
                );
            }
        }
        let latest = previous.unwrap();
        assert!(snapshot
            .get(Key::Blob(latest), &mut [0; 64])
            .unwrap()
            .is_none());
        assert_eq!(snapshot.identity().committed_sequence, Sequence::default());
        assert_eq!(store.checkpoint(deadline()), Err(ports::Error::Busy));
        drop(snapshot);
        let checkpoint = store.checkpoint(deadline());
        eprintln!(
            "large-wal: checkpoint={checkpoint:?} elapsed={:?}",
            started.elapsed()
        );
        assert_eq!(checkpoint, Ok(()));
        assert!(
            started.elapsed() < timeout,
            "large WAL checkpoint timed out"
        );
        assert_eq!(fs::metadata(&wal).unwrap().len(), 0);
        let database = db_path(store.root(), RootEntry::Database).unwrap();
        assert!(fs::metadata(database).unwrap().len() < 64 * 1024 * 1024);
        drop(store);
        let reopened =
            IndexStore::open(&mut root, Arc::new(Timer(AtomicU64::new(1))), 8, deadline()).unwrap();
        reopened.validate_integrity(deadline()).unwrap();
        let mut view = reopened.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(
            view.identity().committed_sequence,
            Sequence::from_u64(sequence)
        );
        let mut input = view
            .open_blob_input(&td_crypto::Provider, latest, MAX_BODY_BYTES)
            .unwrap();
        let mut output = [0; super::super::MAX_FILE_STEP_BYTES];
        while input.position() < input.len() {
            let count = input.read(&mut output).unwrap();
            assert!(count > 0);
            assert!(output[..count].iter().all(|byte| *byte == 0x5a));
        }
        assert_eq!(
            ports::BlobReader::len(&input.finish().unwrap()),
            MAX_BODY_BYTES
        );
        assert!(started.elapsed() < timeout);
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
    fn verified_bodies_refuse_chunks_outside_the_declared_extent() {
        let mut wrong = Vec::new();
        for length in [0usize, 1, 65536, 65537] {
            for extra in [-1i64, (length as u64).div_ceil(65536) as i64, 511] {
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
                let id = BlobId::from_bytes([4; 16]);
                let bytes = vec![0x5a; length];
                let value = encode(Row::Blob(body_row(&bytes, BlobKind::Message)));
                let mut source = std::io::Cursor::new(&bytes);
                store
                    .commit(
                        &td_crypto::Provider,
                        request(0),
                        &[Operation::put(Table::Blobs, id.as_bytes(), &value).unwrap()],
                        &mut [BlobSource {
                            id,
                            source: &mut source,
                        }],
                    )
                    .unwrap();
                let mut old = store.view(ACCOUNT, deadline()).unwrap();
                lock(&store.writer)
                    .unwrap()
                    .native
                    .run(|db| {
                        db.execute_batch("PRAGMA ignore_check_constraints=ON")
                            .map_err(sql)?;
                        db.execute(
                            "INSERT INTO blob_chunks VALUES(?1,?2,?3,?4)",
                            params![
                                ACCOUNT.as_bytes().as_slice(),
                                id.as_bytes().as_slice(),
                                extra,
                                b"extra".as_slice()
                            ],
                        )
                        .map_err(sql)?;
                        db.execute_batch("PRAGMA ignore_check_constraints=OFF")
                            .map_err(sql)
                    })
                    .unwrap();
                let mut current = store.view(ACCOUNT, deadline()).unwrap();
                for (view, corrupted) in [(&mut old, false), (&mut current, true)] {
                    let mut input = view
                        .open_blob_input(&td_crypto::Provider, id, MAX_BODY_BYTES)
                        .unwrap();
                    let mut output = [0; 65536];
                    let mut position = 0;
                    while position < length {
                        let count = input.read(&mut output).unwrap();
                        assert!(count > 0);
                        assert!(output[..count].iter().all(|byte| *byte == 0x5a));
                        position += count;
                    }
                    assert_eq!(input.read(&mut output), Ok(0));
                    let actual = input.finish().map(drop);
                    if corrupted {
                        if actual != Err(ports::Error::Corrupt) {
                            wrong.push((length, extra, actual));
                        }
                    } else {
                        assert_eq!(actual, Ok(()));
                    }
                }
                drop(current);
                drop(old);
                let mut reused = store.view(ACCOUNT, deadline()).unwrap();
                assert_eq!(reused.identity().committed_sequence, Sequence::from_u64(1));
                assert!(reused.get(Key::Blob(id), &mut [0; 64]).unwrap().is_some());
            }
        }
        assert!(wrong.is_empty(), "unexpected chunks verified: {wrong:?}");
    }

    #[test]
    fn body_extent_probes_are_indexed_scoped_and_keep_original_fuel() {
        for fault in ["none", "deadline", "reversal", "budget", "denied"] {
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
            let other = AccountId::from_bytes([8; 16]);
            store.create_account(ACCOUNT, deadline()).unwrap();
            store.create_account(other, deadline()).unwrap();
            let id = BlobId::from_bytes([4; 16]);
            let sibling = BlobId::from_bytes([5; 16]);
            for (account, blob, bytes, expected) in [
                (ACCOUNT, id, b"".as_slice(), 0),
                (ACCOUNT, sibling, b"sibling".as_slice(), 1),
                (other, id, b"other account".as_slice(), 0),
            ] {
                let value = encode(Row::Blob(body_row(bytes, BlobKind::Message)));
                let mut source = std::io::Cursor::new(bytes);
                store
                    .commit(
                        &td_crypto::Provider,
                        CommitRequest {
                            account,
                            ..request(expected)
                        },
                        &[Operation::put(Table::Blobs, blob.as_bytes(), &value).unwrap()],
                        &mut [BlobSource {
                            id: blob,
                            source: &mut source,
                        }],
                    )
                    .unwrap();
            }
            let mut view = store.view(ACCOUNT, deadline()).unwrap();
            let native = view.native().unwrap();
            let plan: Vec<String> = native
                .run(|db| {
                    let mut statement = db
                        .prepare(&format!("EXPLAIN QUERY PLAN {BODY_EXTENT}"))
                        .map_err(sql)?;
                    let rows = statement
                        .query_map(params![1i64, 0i64], |row| row.get(3))
                        .map_err(sql)?;
                    rows.collect::<Result<_, _>>().map_err(sql)
                })
                .unwrap();
            assert_eq!(
                plan.iter()
                    .filter(|step| step.contains("SEARCH c USING PRIMARY KEY"))
                    .count(),
                2,
                "{plan:?}"
            );
            assert!(
                plan.iter().any(|step| step.contains("ordinal<?")),
                "{plan:?}"
            );
            assert!(
                plan.iter().any(|step| step.contains("ordinal>?")),
                "{plan:?}"
            );
            let hook_budget = native.budget.clone();
            let observed = Arc::new(AtomicU64::new(0));
            let hook_observed = observed.clone();
            let hook_clock = clock.clone();
            let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let hook_armed = armed.clone();
            lock(&native.connection)
                .unwrap()
                .authorizer(Some(move |context: rusqlite::hooks::AuthContext<'_>| {
                    if hook_armed.load(Ordering::Relaxed)
                        && matches!(
                            context.action,
                            rusqlite::hooks::AuthAction::Read {
                                table_name: "blob_chunks",
                                ..
                            }
                        )
                    {
                        hook_observed.fetch_add(1, Ordering::Relaxed);
                        match fault {
                            "deadline" => hook_clock.0.store(100, Ordering::Relaxed),
                            "reversal" => hook_clock.0.store(0, Ordering::Relaxed),
                            "budget" => {
                                hook_budget.lock().unwrap().failure = Some(ports::Error::Deadline)
                            }
                            "denied" => return rusqlite::hooks::Authorization::Deny,
                            _ => {}
                        }
                    }
                    rusqlite::hooks::Authorization::Allow
                }))
                .unwrap();
            let input = view
                .open_blob_input(&td_crypto::Provider, id, MAX_BODY_BYTES)
                .unwrap();
            armed.store(true, Ordering::Relaxed);
            let actual = input.finish().map(drop);
            let expected = match fault {
                "none" => Ok(()),
                "deadline" | "budget" => Err(ports::Error::Deadline),
                "reversal" => Err(ports::Error::Invalid),
                "denied" => Err(ports::Error::Io {
                    kind: std::io::ErrorKind::Other,
                    os_code: None,
                }),
                _ => panic!("unknown fault"),
            };
            assert_eq!(actual, expected, "{fault}");
            assert!(observed.load(Ordering::Relaxed) > 0, "{fault}");
            lock(&view.native().unwrap().connection)
                .unwrap()
                .authorizer(
                    None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>,
                )
                .unwrap();
            clock.0.store(1, Ordering::Relaxed);
            if matches!(fault, "deadline" | "reversal" | "budget") {
                assert_eq!(view.get(Key::Blob(id), &mut [0; 64]).map(|_| ()), expected);
            }
            drop(view);
            let mut next = store.view(ACCOUNT, deadline()).unwrap();
            assert_eq!(
                next.open_blob_input(&td_crypto::Provider, id, MAX_BODY_BYTES)
                    .unwrap()
                    .finish()
                    .map(drop),
                Ok(())
            );
        }
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
                    epoch: StoreEpoch::from_bytes([9; 16]),
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
                    epoch: StoreEpoch::from_bytes([9; 16]),
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

    fn commit_encoded_rows(
        store: &IndexStore<'_>,
        request: CommitRequest,
        operations: &[Operation<'_>],
        sources: &mut [BlobSource<'_>],
    ) -> Result<Sequence, CommitError> {
        let length = operations.iter().map(|op| op.encoded_len().unwrap()).sum();
        let mut bytes = vec![0; length];
        let mut offset = 0;
        for operation in operations {
            offset += operation.encode(&mut bytes[offset..]).unwrap();
        }
        let mut slots = vec![None; operations.len()];
        let batch = crate::format::batch::Batch::decode(
            ports::TransactionInput {
                bytes: &bytes,
                count: operations.len(),
            },
            &mut slots,
        )
        .unwrap();
        store.commit_batch(&td_crypto::Provider, request, &batch, sources)
    }

    #[test]
    fn native_email_thread_assignment_survives_typed_and_encoded_replacement() {
        fn commit(
            store: &IndexStore<'_>,
            expected: u64,
            operations: &[Operation<'_>],
            sources: &mut [BlobSource<'_>],
            encoded: bool,
        ) -> Result<Sequence, CommitError> {
            let request = CommitRequest {
                account: ACCOUNT,
                epoch: StoreEpoch::from_bytes([9; 16]),
                expected: Sequence::from_u64(expected),
                utc_ms: 0,
                deadline: deadline(),
            };
            if !encoded {
                return store.commit(&td_crypto::Provider, request, operations, sources);
            }
            commit_encoded_rows(store, request, operations, sources)
        }
        let mut unexpected = Vec::new();
        for encoded in [false, true] {
            for variant in 0..3 {
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
                let blob = BlobId::from_bytes([4; 16]);
                let fresh = BlobId::from_bytes([8; 16]);
                let thread = ThreadId::from_bytes([5; 16]);
                let other = ThreadId::from_bytes([7; 16]);
                let email = EmailId::from_bytes([6; 16]);
                let original = EmailRow {
                    blob,
                    thread,
                    received_at: 0,
                    origin: EmailOrigin::Jmap,
                };
                let original_bytes = encode(Row::Email(original));
                let changed_bytes = encode(Row::Email(EmailRow {
                    thread: other,
                    ..original
                }));
                let thread_bytes = encode(Row::Thread);
                let empty_blob = encode(Row::Blob(BlobRow {
                    kind: BlobKind::Message,
                    length: 0,
                    digest: td_crypto::Provider.sha256().unwrap().finish().unwrap(),
                    created_at: 0,
                }));
                let good =
                    Operation::put(Table::Emails, email.as_bytes(), &original_bytes).unwrap();
                let bad = Operation::put(Table::Emails, email.as_bytes(), &changed_bytes).unwrap();
                let delete = Operation::delete(Table::Emails, email.as_bytes()).unwrap();
                assert_eq!(
                    commit(
                        &store,
                        0,
                        &[
                            Operation::put(Table::Blobs, blob.as_bytes(), &empty_blob).unwrap(),
                            Operation::put(Table::Threads, thread.as_bytes(), &thread_bytes)
                                .unwrap(),
                            Operation::put(Table::Threads, other.as_bytes(), &thread_bytes)
                                .unwrap(),
                            good,
                        ],
                        &mut [BlobSource {
                            id: blob,
                            source: &mut b"".as_slice()
                        }],
                        encoded
                    ),
                    Ok(Sequence::from_u64(1))
                );
                let mut old = store.view(ACCOUNT, deadline()).unwrap();
                let body = b"new body";
                let mut hash = td_crypto::Provider.sha256().unwrap();
                hash.update(body).unwrap();
                let fresh_bytes = encode(Row::Blob(BlobRow {
                    kind: BlobKind::Message,
                    length: body.len() as u64,
                    digest: hash.finish().unwrap(),
                    created_at: 0,
                }));
                let fresh_put =
                    Operation::put(Table::Blobs, fresh.as_bytes(), &fresh_bytes).unwrap();
                let mut operations = vec![fresh_put];
                match variant {
                    0 => operations.push(bad),
                    1 => operations.extend([good, bad]),
                    _ => operations.extend([delete, bad]),
                }
                let mut input = body.as_slice();
                let result = commit(
                    &store,
                    1,
                    &operations,
                    &mut [BlobSource {
                        id: fresh,
                        source: &mut input,
                    }],
                    encoded,
                );
                if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                    unexpected.push((encoded, variant, result));
                    continue;
                }
                assert_eq!(input, body.as_slice(), "history refusal consumed body");
                let mut now = store.view(ACCOUNT, deadline()).unwrap();
                assert_eq!(now.identity().committed_sequence, Sequence::from_u64(1));
                assert_eq!(
                    now.get(Key::Email(email), &mut [0; 512]).unwrap(),
                    Some((Row::Email(original), Sequence::from_u64(1)))
                );
                assert!(now.get(Key::Blob(fresh), &mut [0; 128]).unwrap().is_none());
                drop(now);
                // Superseded thread changes are harmless; the original final assignment survives.
                assert_eq!(
                    commit(
                        &store,
                        1,
                        &[fresh_put, bad, delete, good],
                        &mut [BlobSource {
                            id: fresh,
                            source: &mut input
                        }],
                        encoded
                    ),
                    Ok(Sequence::from_u64(2))
                );
                assert!(input.is_empty());
                // New identities may join another thread; this guard does not select it.
                let new_email = EmailId::from_bytes([10; 16]);
                assert_eq!(
                    commit(
                        &store,
                        2,
                        &[
                            Operation::put(Table::Emails, new_email.as_bytes(), &changed_bytes)
                                .unwrap(),
                            bad,
                            delete,
                        ],
                        &mut [],
                        encoded
                    ),
                    Ok(Sequence::from_u64(3))
                );
                let mut now = store.view(ACCOUNT, deadline()).unwrap();
                assert_eq!(
                    now.get(Key::Email(new_email), &mut [0; 512]).unwrap(),
                    Some((
                        Row::Email(EmailRow {
                            thread: other,
                            ..original
                        }),
                        Sequence::from_u64(3)
                    ))
                );
                assert!(now.get(Key::Email(email), &mut [0; 512]).unwrap().is_none());
                assert_eq!(
                    old.get(Key::Email(email), &mut [0; 512]).unwrap(),
                    Some((Row::Email(original), Sequence::from_u64(1)))
                );
            }
        }
        assert!(
            unexpected.is_empty(),
            "thread assignments changed: {unexpected:?}"
        );
    }

    #[test]
    fn native_maximum_email_history_batch_fits_one_deadline() {
        struct RealClock(std::time::Instant);
        impl Clock for RealClock {
            fn sample(&self) -> Result<Time, ports::Error> {
                Ok(Time {
                    utc_ms: 0,
                    monotonic: Tick(
                        u64::try_from(self.0.elapsed().as_millis())
                            .map_err(|_| ports::Error::Capacity)?,
                    ),
                })
            }
        }
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let clock = Arc::new(RealClock(std::time::Instant::now()));
        let request = |expected| CommitRequest {
            account: ACCOUNT,
            epoch: StoreEpoch::from_bytes([9; 16]),
            expected: Sequence::from_u64(expected),
            utc_ms: 0,
            deadline: Deadline::after(clock.sample().unwrap().monotonic, 30_000).unwrap(),
        };
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            clock.clone(),
            1,
            request(0).deadline,
        )
        .unwrap();
        store.create_account(ACCOUNT, request(0).deadline).unwrap();
        let blob = BlobId::from_bytes([4; 16]);
        let thread = ThreadId::from_bytes([5; 16]);
        let blob_bytes = encode(Row::Blob(BlobRow {
            kind: BlobKind::Message,
            length: 0,
            digest: td_crypto::Provider.sha256().unwrap().finish().unwrap(),
            created_at: 0,
        }));
        let thread_bytes = encode(Row::Thread);
        store
            .commit(
                &td_crypto::Provider,
                request(0),
                &[
                    Operation::put(Table::Blobs, blob.as_bytes(), &blob_bytes).unwrap(),
                    Operation::put(Table::Threads, thread.as_bytes(), &thread_bytes).unwrap(),
                ],
                &mut [BlobSource {
                    id: blob,
                    source: &mut b"".as_slice(),
                }],
            )
            .unwrap();
        let ids: Vec<_> = (0u128..4096)
            .map(|id| EmailId::from_bytes(id.to_be_bytes()))
            .collect();
        let email = encode(Row::Email(EmailRow {
            blob,
            thread,
            received_at: 0,
            origin: EmailOrigin::Jmap,
        }));
        let operations: Vec<_> = ids
            .iter()
            .map(|id| Operation::put(Table::Emails, id.as_bytes(), &email).unwrap())
            .collect();
        assert_eq!(
            store.commit(&td_crypto::Provider, request(1), &operations, &mut []),
            Ok(Sequence::from_u64(2))
        );
        let started = std::time::Instant::now();
        assert_eq!(
            commit_encoded_rows(&store, request(2), &operations, &mut []),
            Ok(Sequence::from_u64(3))
        );
        eprintln!(
            "4096 Email history PUTs: {:?}; debug_assertions={}",
            started.elapsed(),
            cfg!(debug_assertions)
        );
        let mut view = store.view(ACCOUNT, request(3).deadline).unwrap();
        for id in ids {
            assert_eq!(
                view.get(Key::Email(id), &mut [0; 512]).unwrap(),
                Some((
                    Row::decode(Table::Emails, &email).unwrap(),
                    Sequence::from_u64(3)
                ))
            );
        }
    }

    #[test]
    fn maintenance_view_keeps_one_allowance_and_resets_the_returned_pool_slot() {
        use crate::store_fs::{BodyCheckLimits, MAX_FILE_STEP_BYTES};
        const WORK: &str = "WITH RECURSIVE work(n) AS (VALUES(0) UNION ALL SELECT n+1 FROM work WHERE n<1000000) SELECT max(n) FROM work";
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let clock = Arc::new(Timer(AtomicU64::new(1)));
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            clock.clone(),
            1,
            deadline(),
        )
        .unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        for mode in ["normal", "maintenance", "reused"] {
            let mut view = if mode == "maintenance" {
                store.maintenance_view(ACCOUNT, deadline()).unwrap()
            } else {
                store.view(ACCOUNT, deadline()).unwrap()
            };
            assert!(matches!(
                store.view(ACCOUNT, deadline()),
                Err(ports::Error::Busy)
            ));
            assert!(matches!(
                store.maintenance_view(ACCOUNT, deadline()),
                Err(ports::Error::Busy)
            ));
            let result: Result<i64, _> = view.read_snapshot(|native| {
                native.run(|db| db.query_row(WORK, [], |row| row.get(0)).map_err(sql))
            });
            let remaining = lock(&view.native().unwrap().budget).unwrap().remaining;
            if mode == "maintenance" {
                assert_eq!(result, Ok(1000000));
                assert!(INTEGRITY_VM_STEPS - remaining > VM_STEPS);
                let report = view
                    .verify_bodies(
                        &td_crypto::Provider,
                        BodyCheckLimits { blobs: 0, bytes: 0 },
                        &mut [0; MAX_FILE_STEP_BYTES],
                    )
                    .unwrap();
                assert_eq!((report.blobs(), report.bytes()), (0, 0));
                let after = lock(&view.native().unwrap().budget).unwrap().remaining;
                assert!(after < remaining);
                assert_eq!(view.get(Key::Mailbox(ID), &mut [0; 128]), Ok(None));
                assert!(lock(&view.native().unwrap().budget).unwrap().remaining < after);
                clock.0.store(100, Ordering::Relaxed);
                assert_eq!(
                    view.get(Key::Mailbox(ID), &mut [0; 128]),
                    Err(ports::Error::Deadline)
                );
                clock.0.store(1, Ordering::Relaxed);
                assert_eq!(
                    view.get(Key::Mailbox(ID), &mut [0; 128]),
                    Err(ports::Error::Deadline)
                );
            } else {
                assert_eq!(result, Err(ports::Error::Capacity), "{mode}");
                assert_eq!(remaining, 0, "{mode}");
                assert_eq!(
                    view.get(Key::Mailbox(ID), &mut [0; 128]),
                    Err(ports::Error::Capacity),
                    "{mode}"
                );
            }
            drop(view);
        }
    }

    #[test]
    fn maintenance_view_reuses_snapshot_loss_and_missing_account_cleanup() {
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
        assert!(matches!(
            store.maintenance_view(AccountId::from_bytes([11; 16]), deadline()),
            Err(ports::Error::NotFound)
        ));
        let mut view = store.maintenance_view(ACCOUNT, deadline()).unwrap();
        let identity = view.identity();
        lock(&view.native().unwrap().connection)
            .unwrap()
            .execute_batch("ROLLBACK")
            .unwrap();
        assert_eq!(
            view.get(Key::Mailbox(ID), &mut [0; 128]),
            Err(ports::Error::Corrupt)
        );
        lock(&view.native().unwrap().connection)
            .unwrap()
            .execute_batch("BEGIN DEFERRED")
            .unwrap();
        assert_eq!(
            view.get(Key::Mailbox(ID), &mut [0; 128]),
            Err(ports::Error::Corrupt)
        );
        drop(view);
        assert!(matches!(
            store.maintenance_view(ACCOUNT, deadline()),
            Err(ports::Error::Busy)
        ));
        drop(store);
        let store = IndexStore::open(&mut root, clock, 1, deadline()).unwrap();
        let view = store.maintenance_view(ACCOUNT, deadline()).unwrap();
        assert_eq!(view.identity(), identity);
    }

    fn account_check_limits() -> crate::store_fs::AccountCheckLimits {
        crate::store_fs::AccountCheckLimits {
            metadata: crate::metadata_sweep::Limits {
                rows: 5,
                parent_reads: 3,
            },
            bodies: crate::store_fs::BodyCheckLimits {
                blobs: 3,
                bytes: 65539,
            },
        }
    }

    #[test]
    fn account_verification_preserves_old_wal_reports_and_all_explicit_limits() {
        use crate::store_fs::{AccountCheckError, MAX_FILE_STEP_BYTES};
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
        seed_body_verification(&store, ACCOUNT);
        let parent = MailboxId::from_bytes([7; 16]);
        let child = MailboxId::from_bytes([8; 16]);
        store
            .commit(
                &td_crypto::Provider,
                request(3),
                &[
                    Operation::put(
                        Table::Mailboxes,
                        parent.as_bytes(),
                        &mailbox("parent", None),
                    )
                    .unwrap(),
                    Operation::put(
                        Table::Mailboxes,
                        child.as_bytes(),
                        &mailbox("child", Some(parent)),
                    )
                    .unwrap(),
                ],
                &mut [],
            )
            .unwrap();
        let mut old = store.maintenance_view(ACCOUNT, deadline()).unwrap();
        let original = old.identity();
        let replacement = BlobId::from_bytes([10; 16]);
        let value = encode(Row::Blob(body_row(b"yy", BlobKind::Message)));
        let mut source = b"yy".as_slice();
        store
            .commit(
                &td_crypto::Provider,
                request(4),
                &[
                    Operation::delete(Table::Blobs, BlobId::from_bytes([5; 16]).as_bytes())
                        .unwrap(),
                    Operation::put(Table::Blobs, replacement.as_bytes(), &value).unwrap(),
                ],
                &mut [BlobSource {
                    id: replacement,
                    source: &mut source,
                }],
            )
            .unwrap();
        let mut scratch = [0; MAX_FILE_STEP_BYTES];
        let report = old
            .verify_account(
                &td_crypto::Provider,
                17,
                account_check_limits(),
                &mut scratch,
            )
            .unwrap();
        assert_eq!(report.identity(), original);
        assert_eq!(report.metadata().references().utc_ms(), 17);
        assert_eq!(report.metadata().references().rows(), 5);
        assert_eq!(report.metadata().mailboxes().mailboxes(), 2);
        assert_eq!(report.metadata().mailboxes().reads(), 3);
        assert_eq!(
            (report.bodies().blobs(), report.bodies().bytes()),
            (3, 65538)
        );
        drop(old);
        let mut fresh = store.view(ACCOUNT, deadline()).unwrap();
        let report = fresh
            .verify_account(
                &td_crypto::Provider,
                17,
                account_check_limits(),
                &mut scratch,
            )
            .unwrap();
        assert_eq!(report.identity().committed_sequence, Sequence::from_u64(5));
        assert_eq!(
            (report.bodies().blobs(), report.bodies().bytes()),
            (3, 65539)
        );
        drop(fresh);
        for fault in ["rows", "parents", "blobs", "bytes"] {
            let mut limits = account_check_limits();
            match fault {
                "rows" => limits.metadata.rows = 4,
                "parents" => limits.metadata.parent_reads = 2,
                "blobs" => limits.bodies.blobs = 2,
                "bytes" => limits.bodies.bytes = 65538,
                _ => panic!("unknown fault"),
            }
            let mut view = store.maintenance_view(ACCOUNT, deadline()).unwrap();
            let result = view.verify_account(&td_crypto::Provider, 17, limits, &mut scratch);
            let expected = match fault {
                "rows" => AccountCheckError::Metadata(crate::metadata_sweep::Error::References(
                    crate::reference_sweep::Error::RowLimit,
                )),
                "parents" => AccountCheckError::Metadata(crate::metadata_sweep::Error::Mailboxes(
                    crate::mailbox_sweep::Error::Parent(crate::mailbox_parents::Error::ReadLimit),
                )),
                _ => AccountCheckError::Bodies(ports::Error::Capacity),
            };
            assert_eq!(result, Err(expected), "{fault}");
        }
        let empty = AccountId::from_bytes([11; 16]);
        store.create_account(empty, deadline()).unwrap();
        let mut view = store.maintenance_view(empty, deadline()).unwrap();
        let mut limits = account_check_limits();
        limits.metadata.rows = 0;
        limits.metadata.parent_reads = 0;
        limits.bodies.blobs = 0;
        limits.bodies.bytes = 0;
        let report = view
            .verify_account(&td_crypto::Provider, 17, limits, &mut scratch)
            .unwrap();
        assert_eq!(report.identity().account, empty);
        assert_eq!(
            (
                report.metadata().references().rows(),
                report.bodies().bytes()
            ),
            (0, 0)
        );
    }

    #[test]
    fn account_verification_stops_at_metadata_failure_before_body_work() {
        use crate::store_fs::{AccountCheckError, MAX_FILE_STEP_BYTES};
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
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
        seed_body_verification(&store, ACCOUNT);
        {
            let writer = lock(&store.writer).unwrap();
            writer.native.run(|db| {
                db.execute("UPDATE blob_chunks SET body=x'7a' WHERE account=?1 AND blob=?2 AND ordinal=0", params![ACCOUNT.as_bytes().as_slice(), BlobId::from_bytes([5;16]).as_bytes().as_slice()]).map_err(sql)?;
                Ok(())
            }).unwrap();
        }
        store.validate_integrity(deadline()).unwrap();
        let mut view = store.maintenance_view(ACCOUNT, deadline()).unwrap();
        let reads = Arc::new(AtomicU64::new(0));
        let observed = reads.clone();
        lock(&view.native().unwrap().connection)
            .unwrap()
            .authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::Read {
                        table_name: "blob_chunks",
                        ..
                    }
                ) {
                    observed.fetch_add(1, Ordering::Relaxed);
                }
                Authorization::Allow
            }))
            .unwrap();
        let mut scratch = [0; MAX_FILE_STEP_BYTES];
        let mut limits = account_check_limits();
        limits.metadata.rows = 0;
        assert_eq!(
            view.verify_account(&td_crypto::Provider, 0, limits, &mut scratch),
            Err(AccountCheckError::Metadata(
                crate::metadata_sweep::Error::References(crate::reference_sweep::Error::RowLimit)
            ))
        );
        assert_eq!(reads.load(Ordering::Relaxed), 0);
        assert_eq!(
            view.verify_account(
                &td_crypto::Provider,
                0,
                account_check_limits(),
                &mut scratch
            ),
            Err(AccountCheckError::Bodies(ports::Error::Corrupt))
        );
        assert!(reads.load(Ordering::Relaxed) > 0);
        lock(&view.native().unwrap().connection)
            .unwrap()
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
    }

    #[test]
    fn account_verification_keeps_one_native_scope_across_metadata_and_bodies() {
        use crate::store_fs::{AccountCheckError, MAX_FILE_STEP_BYTES};
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        for fault in ["fuel", "deadline", "reversal", "snapshot"] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let clock = Arc::new(Timer(AtomicU64::new(1)));
            let store = IndexStore::create(
                &mut root,
                StoreEpoch::from_bytes([9; 16]),
                clock.clone(),
                1,
                deadline(),
            )
            .unwrap();
            seed_body_verification(&store, ACCOUNT);
            let mut view = store.maintenance_view(ACCOUNT, deadline()).unwrap();
            let native = view.native().unwrap();
            let budget = native.budget.clone();
            let initial = lock(&budget).unwrap().remaining;
            let hook_budget = budget.clone();
            let hook_clock = clock.clone();
            let seen = Arc::new(AtomicU64::new(0));
            let hook_seen = seen.clone();
            if fault == "snapshot" {
                lock(&native.connection)
                    .unwrap()
                    .execute_batch("ROLLBACK")
                    .unwrap();
            }
            lock(&native.connection)
                .unwrap()
                .authorizer(Some(move |context: AuthContext<'_>| {
                    if matches!(
                        context.action,
                        AuthAction::Read {
                            table_name: "blob_chunks",
                            ..
                        }
                    ) && hook_seen.fetch_add(1, Ordering::Relaxed) == 0
                    {
                        assert!(hook_budget.lock().unwrap().remaining < initial);
                        match fault {
                            "fuel" => hook_budget.lock().unwrap().remaining = 0,
                            "deadline" => hook_clock.0.store(100, Ordering::Relaxed),
                            "reversal" => hook_clock.0.store(0, Ordering::Relaxed),
                            _ => (),
                        }
                    }
                    Authorization::Allow
                }))
                .unwrap();
            let expected = match fault {
                "fuel" => ports::Error::Capacity,
                "deadline" => ports::Error::Deadline,
                "reversal" => ports::Error::Invalid,
                _ => ports::Error::Corrupt,
            };
            let wrapped = if fault == "snapshot" {
                AccountCheckError::Metadata(crate::metadata_sweep::Error::References(
                    crate::reference_sweep::Error::View(expected),
                ))
            } else {
                AccountCheckError::Bodies(expected)
            };
            let mut scratch = [0; MAX_FILE_STEP_BYTES];
            assert_eq!(
                view.verify_account(
                    &td_crypto::Provider,
                    0,
                    account_check_limits(),
                    &mut scratch
                ),
                Err(wrapped),
                "{fault}"
            );
            assert_eq!(seen.load(Ordering::Relaxed) > 0, fault != "snapshot");
            lock(&view.native().unwrap().connection)
                .unwrap()
                .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .unwrap();
            clock.0.store(1, Ordering::Relaxed);
            if fault == "snapshot" {
                lock(&view.native().unwrap().connection)
                    .unwrap()
                    .execute_batch("BEGIN DEFERRED")
                    .unwrap();
            } else {
                assert_eq!(lock(&budget).unwrap().failure, Some(expected));
            }
            assert_eq!(
                view.verify_account(
                    &td_crypto::Provider,
                    0,
                    account_check_limits(),
                    &mut scratch
                ),
                Err(AccountCheckError::Metadata(
                    crate::metadata_sweep::Error::References(crate::reference_sweep::Error::View(
                        expected
                    ))
                ))
            );
            drop(view);
            if fault == "snapshot" {
                assert!(matches!(
                    store.maintenance_view(ACCOUNT, deadline()),
                    Err(ports::Error::Busy)
                ));
                drop(store);
                let reopened = IndexStore::open(&mut root, clock.clone(), 1, deadline()).unwrap();
                assert!(reopened
                    .maintenance_view(ACCOUNT, deadline())
                    .unwrap()
                    .verify_account(
                        &td_crypto::Provider,
                        0,
                        account_check_limits(),
                        &mut scratch
                    )
                    .is_ok());
            } else {
                assert!(store
                    .maintenance_view(ACCOUNT, deadline())
                    .unwrap()
                    .verify_account(
                        &td_crypto::Provider,
                        0,
                        account_check_limits(),
                        &mut scratch
                    )
                    .is_ok());
            }
        }
    }

    fn seed_body_verification(store: &IndexStore<'_>, account: AccountId) {
        store.create_account(account, deadline()).unwrap();
        for (sequence, (tag, length, kind)) in [
            (4, 0, BlobKind::Message),
            (5, 1, BlobKind::Upload),
            (6, 65537, BlobKind::Message),
        ]
        .into_iter()
        .enumerate()
        {
            let id = BlobId::from_bytes([tag; 16]);
            let bytes = vec![b'x'; length];
            let value = encode(Row::Blob(body_row(&bytes, kind)));
            let mut source = bytes.as_slice();
            store
                .commit(
                    &td_crypto::Provider,
                    CommitRequest {
                        account,
                        ..request(sequence as u64)
                    },
                    &[Operation::put(Table::Blobs, id.as_bytes(), &value).unwrap()],
                    &mut [BlobSource {
                        id,
                        source: &mut source,
                    }],
                )
                .unwrap();
        }
    }

    #[test]
    fn complete_body_verification_reports_one_snapshot_and_enforces_work_limits() {
        use crate::store_fs::{BodyCheckLimits, MAX_FILE_STEP_BYTES};
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = IndexStore::create(
            &mut root,
            StoreEpoch::from_bytes([9; 16]),
            Arc::new(Timer(AtomicU64::new(1))),
            3,
            deadline(),
        )
        .unwrap();
        seed_body_verification(&store, ACCOUNT);
        let other = AccountId::from_bytes([11; 16]);
        let empty_account = AccountId::from_bytes([12; 16]);
        store.create_account(other, deadline()).unwrap();
        store.create_account(empty_account, deadline()).unwrap();
        let id = BlobId::from_bytes([4; 16]);
        let value = encode(Row::Blob(body_row(b"other", BlobKind::Upload)));
        let mut source = b"other".as_slice();
        store
            .commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: other,
                    ..request(0)
                },
                &[Operation::put(Table::Blobs, id.as_bytes(), &value).unwrap()],
                &mut [BlobSource {
                    id,
                    source: &mut source,
                }],
            )
            .unwrap();
        let mut old = store.view(ACCOUNT, deadline()).unwrap();
        let original = old.identity();
        let replacement = BlobId::from_bytes([7; 16]);
        let value = encode(Row::Blob(body_row(b"yy", BlobKind::Message)));
        let mut source = b"yy".as_slice();
        store
            .commit(
                &td_crypto::Provider,
                request(3),
                &[
                    Operation::delete(Table::Blobs, BlobId::from_bytes([5; 16]).as_bytes())
                        .unwrap(),
                    Operation::put(Table::Blobs, replacement.as_bytes(), &value).unwrap(),
                ],
                &mut [BlobSource {
                    id: replacement,
                    source: &mut source,
                }],
            )
            .unwrap();
        let mut scratch = [0; MAX_FILE_STEP_BYTES];
        let report = old
            .verify_bodies(
                &td_crypto::Provider,
                BodyCheckLimits {
                    blobs: 3,
                    bytes: 65538,
                },
                &mut scratch,
            )
            .unwrap();
        assert_eq!(
            (report.identity(), report.blobs(), report.bytes()),
            (original, 3, 65538)
        );
        assert_eq!(old.identity(), original);
        drop(old);
        let mut fresh = store.view(ACCOUNT, deadline()).unwrap();
        let report = fresh
            .verify_bodies(
                &td_crypto::Provider,
                BodyCheckLimits {
                    blobs: 3,
                    bytes: 65539,
                },
                &mut scratch,
            )
            .unwrap();
        assert_eq!((report.blobs(), report.bytes()), (3, 65539));
        assert_eq!(report.identity().committed_sequence, Sequence::from_u64(4));
        drop(fresh);
        let mut isolated = store.view(other, deadline()).unwrap();
        let report = isolated
            .verify_bodies(
                &td_crypto::Provider,
                BodyCheckLimits { blobs: 1, bytes: 5 },
                &mut scratch,
            )
            .unwrap();
        assert_eq!(
            (report.identity().account, report.blobs(), report.bytes()),
            (other, 1, 5)
        );
        drop(isolated);
        let mut empty = store.view(empty_account, deadline()).unwrap();
        let report = empty
            .verify_bodies(
                &td_crypto::Provider,
                BodyCheckLimits { blobs: 0, bytes: 0 },
                &mut scratch,
            )
            .unwrap();
        assert_eq!((report.blobs(), report.bytes()), (0, 0));
        drop(empty);
        for limits in [
            BodyCheckLimits { blobs: 0, bytes: 0 },
            BodyCheckLimits {
                blobs: 2,
                bytes: 65539,
            },
            BodyCheckLimits {
                blobs: 3,
                bytes: 65538,
            },
        ] {
            let mut view = store.view(ACCOUNT, deadline()).unwrap();
            assert_eq!(
                view.verify_bodies(&td_crypto::Provider, limits, &mut scratch),
                Err(ports::Error::Capacity)
            );
        }
    }

    #[test]
    fn complete_body_verification_refuses_same_size_digest_damage_and_empty_extras() {
        use crate::store_fs::{BodyCheckLimits, MAX_FILE_STEP_BYTES};
        for (tag, ordinal, extra) in [(5, 0, false), (6, 0, false), (6, 1, false), (4, 0, true)] {
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
            seed_body_verification(&store, ACCOUNT);
            let mut old = store.view(ACCOUNT, deadline()).unwrap();
            let id = BlobId::from_bytes([tag; 16]);
            {
                let writer = lock(&store.writer).unwrap();
                writer.native.run(|db| {
                    if extra {
                        db.execute("INSERT INTO blob_chunks(account,blob,ordinal,body) VALUES(?1,?2,0,x'7a')", params![ACCOUNT.as_bytes().as_slice(), id.as_bytes().as_slice()]).map_err(sql)?;
                    } else {
                        let mut bytes: Vec<u8> = db.query_row("SELECT body FROM blob_chunks WHERE account=?1 AND blob=?2 AND ordinal=?3", params![ACCOUNT.as_bytes().as_slice(), id.as_bytes().as_slice(), ordinal], |row| row.get(0)).map_err(sql)?;
                        bytes.fill(b'z');
                        assert_eq!(db.execute("UPDATE blob_chunks SET body=?4 WHERE account=?1 AND blob=?2 AND ordinal=?3", params![ACCOUNT.as_bytes().as_slice(), id.as_bytes().as_slice(), ordinal, bytes]).map_err(sql)?, 1);
                    }
                    Ok(())
                }).unwrap();
            }
            assert_eq!(
                store.validate_integrity(deadline()),
                if extra {
                    Err(ports::Error::Corrupt)
                } else {
                    Ok(())
                }
            );
            let limits = BodyCheckLimits {
                blobs: 3,
                bytes: 65538,
            };
            let mut scratch = [0; MAX_FILE_STEP_BYTES];
            let report = old
                .verify_bodies(&td_crypto::Provider, limits, &mut scratch)
                .unwrap();
            assert_eq!((report.blobs(), report.bytes()), (3, 65538));
            let mut fresh = store.view(ACCOUNT, deadline()).unwrap();
            assert_eq!(
                fresh.verify_bodies(&td_crypto::Provider, limits, &mut scratch),
                Err(ports::Error::Corrupt)
            );
        }
    }

    #[test]
    fn complete_body_verification_keeps_native_refusals_and_snapshot_custody() {
        use crate::store_fs::{BodyCheckLimits, MAX_FILE_STEP_BYTES};
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        for fault in ["none", "deadline", "reversal", "fuel", "denied", "snapshot"] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let clock = Arc::new(Timer(AtomicU64::new(1)));
            let store = IndexStore::create(
                &mut root,
                StoreEpoch::from_bytes([9; 16]),
                clock.clone(),
                1,
                deadline(),
            )
            .unwrap();
            seed_body_verification(&store, ACCOUNT);
            let mut view = store.view(ACCOUNT, deadline()).unwrap();
            let native = view.native().unwrap();
            let budget = native.budget.clone();
            let hook_budget = budget.clone();
            let observed = Arc::new(AtomicU64::new(0));
            let hook_observed = observed.clone();
            let hook_clock = clock.clone();
            if fault == "snapshot" {
                lock(&native.connection)
                    .unwrap()
                    .execute_batch("ROLLBACK")
                    .unwrap();
            }
            lock(&native.connection)
                .unwrap()
                .authorizer(Some(move |context: AuthContext<'_>| {
                    if matches!(
                        context.action,
                        AuthAction::Read {
                            table_name: "blob_chunks",
                            column_name: "body"
                        }
                    ) {
                        hook_observed.fetch_add(1, Ordering::Relaxed);
                        match fault {
                            "deadline" => hook_clock.0.store(100, Ordering::Relaxed),
                            "reversal" => hook_clock.0.store(0, Ordering::Relaxed),
                            "fuel" => hook_budget.lock().unwrap().remaining = 0,
                            "denied" => return Authorization::Deny,
                            _ => (),
                        }
                    }
                    Authorization::Allow
                }))
                .unwrap();
            let limits = BodyCheckLimits {
                blobs: 3,
                bytes: 65538,
            };
            let mut scratch = [0; MAX_FILE_STEP_BYTES];
            let result = view.verify_bodies(&td_crypto::Provider, limits, &mut scratch);
            let expected = match fault {
                "none" => None,
                "deadline" => Some(ports::Error::Deadline),
                "reversal" => Some(ports::Error::Invalid),
                "fuel" => Some(ports::Error::Capacity),
                "denied" => Some(ports::Error::Io {
                    kind: std::io::ErrorKind::Other,
                    os_code: None,
                }),
                "snapshot" => Some(ports::Error::Corrupt),
                _ => panic!("unknown fault"),
            };
            assert_eq!(result.as_ref().err().copied(), expected, "{fault}");
            assert_eq!(
                observed.load(Ordering::Relaxed) > 0,
                fault != "snapshot",
                "{fault}"
            );
            lock(&view.native().unwrap().connection)
                .unwrap()
                .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .unwrap();
            clock.0.store(1, Ordering::Relaxed);
            if matches!(fault, "deadline" | "reversal" | "fuel") {
                assert_eq!(lock(&budget).unwrap().failure, expected, "{fault}");
                assert_eq!(
                    view.verify_bodies(&td_crypto::Provider, limits, &mut scratch)
                        .err(),
                    expected,
                    "{fault}"
                );
            }
            if fault == "snapshot" {
                lock(&view.native().unwrap().connection)
                    .unwrap()
                    .execute_batch("BEGIN DEFERRED")
                    .unwrap();
                assert_eq!(
                    view.verify_bodies(&td_crypto::Provider, limits, &mut scratch),
                    Err(ports::Error::Corrupt)
                );
            }
            drop(view);
            if fault == "snapshot" {
                assert!(matches!(
                    store.view(ACCOUNT, deadline()),
                    Err(ports::Error::Busy)
                ));
            } else {
                let mut healthy = store.view(ACCOUNT, deadline()).unwrap();
                assert!(healthy
                    .verify_bodies(&td_crypto::Provider, limits, &mut scratch)
                    .is_ok());
            }
        }
    }

    #[test]
    fn body_geometry_maintenance_uses_scoped_probes_and_original_work_scope() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        for fault in ["none", "deadline", "reversal", "fuel", "denied"] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let clock = Arc::new(Timer(AtomicU64::new(1)));
            let store = IndexStore::create(
                &mut root,
                StoreEpoch::from_bytes([9; 16]),
                clock.clone(),
                1,
                deadline(),
            )
            .unwrap();
            store.create_account(ACCOUNT, deadline()).unwrap();
            let (budget, observed) = {
                let writer = lock(&store.writer).unwrap();
                let plan: Vec<String> = writer
                    .native
                    .run(|db| {
                        let mut statement = db
                            .prepare(&format!("EXPLAIN QUERY PLAN {BODY_GEOMETRY}"))
                            .map_err(sql)?;
                        let rows = statement.query_map([], |row| row.get(3)).map_err(sql)?;
                        rows.collect::<Result<_, _>>().map_err(sql)
                    })
                    .unwrap();
                assert_eq!(
                    plan.iter()
                        .filter(|step| step
                            .contains("SEARCH c USING PRIMARY KEY (account=? AND blob=?)"))
                        .count(),
                    2,
                    "{plan:?}"
                );
                assert!(
                    !plan.iter().any(|step| step.contains("TEMP B-TREE")),
                    "{plan:?}"
                );
                let budget = writer.native.budget.clone();
                let hook_budget = budget.clone();
                let observed = Arc::new(AtomicU64::new(0));
                let hook_observed = observed.clone();
                let hook_clock = clock.clone();
                lock(&writer.native.connection)
                    .unwrap()
                    .authorizer(Some(move |context: AuthContext<'_>| {
                        if matches!(
                            context.action,
                            AuthAction::Read {
                                table_name: "blob_chunks",
                                column_name: "ordinal"
                            }
                        ) {
                            hook_observed.fetch_add(1, Ordering::Relaxed);
                            match fault {
                                "deadline" => hook_clock.0.store(100, Ordering::Relaxed),
                                "reversal" => hook_clock.0.store(0, Ordering::Relaxed),
                                "fuel" => hook_budget.lock().unwrap().remaining = 0,
                                "denied" => return Authorization::Deny,
                                _ => (),
                            }
                        }
                        Authorization::Allow
                    }))
                    .unwrap();
                (budget, observed)
            };
            let expected = match fault {
                "none" => Ok(()),
                "deadline" => Err(ports::Error::Deadline),
                "reversal" => Err(ports::Error::Invalid),
                "fuel" => Err(ports::Error::Capacity),
                "denied" => Err(ports::Error::Io {
                    kind: std::io::ErrorKind::Other,
                    os_code: None,
                }),
                _ => panic!("unknown fault"),
            };
            assert_eq!(store.validate_integrity(deadline()), expected, "{fault}");
            assert!(observed.load(Ordering::Relaxed) > 0, "{fault}");
            {
                let writer = lock(&store.writer).unwrap();
                assert!(!writer.stopped);
                lock(&writer.native.connection)
                    .unwrap()
                    .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                    .unwrap();
            }
            clock.0.store(1, Ordering::Relaxed);
            if matches!(fault, "deadline" | "reversal" | "fuel") {
                assert_eq!(lock(&budget).unwrap().failure, expected.err(), "{fault}");
            }
            assert_eq!(store.validate_integrity(deadline()), Ok(()), "{fault}");
        }
    }

    #[test]
    fn integrity_maintenance_refuses_valid_sqlite_with_malformed_body_geometry() {
        const DELETE: &str = "DELETE FROM blob_chunks WHERE account=?1 AND blob=?2 AND ordinal=?3";
        const INSERT: &str =
            "INSERT INTO blob_chunks(account,blob,ordinal,body) VALUES(?1,?2,?3,?4)";
        const SIZE: &str =
            "UPDATE blob_chunks SET body=?4 WHERE account=?1 AND blob=?2 AND ordinal=?3";
        const SHIFT: &str =
            "UPDATE blob_chunks SET ordinal=?4 WHERE account=?1 AND blob=?2 AND ordinal=?3";
        let other = AccountId::from_bytes([11; 16]);
        let id = BlobId::from_bytes([4; 16]);
        let sibling = BlobId::from_bytes([5; 16]);
        let mut unexpected = Vec::new();
        for (length, shape) in [
            (0, "extra"),
            (1, "missing"),
            (1, "extra"),
            (65536, "short"),
            (65536, "extra"),
            (65537, "missing"),
            (65537, "swap"),
            (65537, "shift"),
            (65537, "distant"),
        ] {
            for damaged in [ACCOUNT, other] {
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
                let bytes = vec![b'x'; length];
                for account in [ACCOUNT, other] {
                    store.create_account(account, deadline()).unwrap();
                    let body = if account == damaged {
                        bytes.as_slice()
                    } else {
                        b"another account".as_slice()
                    };
                    let value = encode(Row::Blob(body_row(
                        body,
                        if account == ACCOUNT {
                            BlobKind::Message
                        } else {
                            BlobKind::Upload
                        },
                    )));
                    let mut source = body;
                    store
                        .commit(
                            &td_crypto::Provider,
                            CommitRequest {
                                account,
                                ..request(0)
                            },
                            &[Operation::put(Table::Blobs, id.as_bytes(), &value).unwrap()],
                            &mut [BlobSource {
                                id,
                                source: &mut source,
                            }],
                        )
                        .unwrap();
                }
                let value = encode(Row::Blob(body_row(b"", BlobKind::Message)));
                let mut empty = b"".as_slice();
                store
                    .commit(
                        &td_crypto::Provider,
                        CommitRequest {
                            account: damaged,
                            ..request(1)
                        },
                        &[Operation::put(Table::Blobs, sibling.as_bytes(), &value).unwrap()],
                        &mut [BlobSource {
                            id: sibling,
                            source: &mut empty,
                        }],
                    )
                    .unwrap();
                assert_eq!(store.validate_integrity(deadline()), Ok(()));
                let mut retained = store.view(damaged, deadline()).unwrap();
                {
                    let writer = lock(&store.writer).unwrap();
                    writer
                        .native
                        .run(|db| {
                            let account = damaged.as_bytes().as_slice();
                            let blob = id.as_bytes().as_slice();
                            match shape {
                                "missing" => {
                                    assert_eq!(
                                        db.execute(DELETE, params![account, blob, 0i64])
                                            .map_err(sql)?,
                                        1
                                    );
                                }
                                "extra" | "distant" => {
                                    let ordinal = if shape == "distant" {
                                        511i64
                                    } else {
                                        length.div_ceil(65536) as i64
                                    };
                                    assert_eq!(
                                        db.execute(
                                            INSERT,
                                            params![account, blob, ordinal, b"z".as_slice()]
                                        )
                                        .map_err(sql)?,
                                        1
                                    );
                                }
                                "short" => {
                                    assert_eq!(
                                        db.execute(
                                            SIZE,
                                            params![account, blob, 0i64, vec![b'x'; 65535]]
                                        )
                                        .map_err(sql)?,
                                        1
                                    );
                                }
                                "swap" => {
                                    assert_eq!(
                                        db.execute(
                                            SIZE,
                                            params![account, blob, 0i64, b"x".as_slice()]
                                        )
                                        .map_err(sql)?,
                                        1
                                    );
                                    assert_eq!(
                                        db.execute(
                                            SIZE,
                                            params![account, blob, 1i64, vec![b'x'; 65536]]
                                        )
                                        .map_err(sql)?,
                                        1
                                    );
                                }
                                "shift" => {
                                    assert_eq!(
                                        db.execute(SHIFT, params![account, blob, 1i64, 2i64])
                                            .map_err(sql)?,
                                        1
                                    );
                                }
                                _ => panic!("unknown shape"),
                            }
                            let physical: String = db
                                .query_row("PRAGMA integrity_check(1)", [], |row| row.get(0))
                                .map_err(sql)?;
                            assert_eq!(physical, "ok", "{length}/{shape}");
                            let mut statement =
                                db.prepare("PRAGMA foreign_key_check").map_err(sql)?;
                            assert!(statement
                                .query([])
                                .map_err(sql)?
                                .next()
                                .map_err(sql)?
                                .is_none());
                            Ok(())
                        })
                        .unwrap();
                }
                let actual = store.validate_integrity(deadline());
                if actual != Err(ports::Error::Corrupt) {
                    unexpected.push((damaged, length, shape, actual));
                }
                // The pre-corruption WAL snapshot still owns the complete original body.
                let mut input = retained
                    .open_blob_input(&td_crypto::Provider, id, MAX_BODY_BYTES)
                    .unwrap();
                let mut output = vec![0; 65536];
                let mut offset = 0;
                loop {
                    let count = input.read(&mut output).unwrap();
                    assert_eq!(&output[..count], &bytes[offset..offset + count]);
                    offset += count;
                    if count == 0 {
                        break;
                    }
                }
                assert_eq!(offset, bytes.len());
                input.finish().unwrap();
            }
        }
        assert!(
            unexpected.is_empty(),
            "malformed chunk layouts accepted by maintenance: {unexpected:?}"
        );
    }

    #[test]
    fn integrity_maintenance_checks_index_contents_before_trusting_anchor_scan() {
        use std::io::{Read, Seek, SeekFrom, Write};
        const ROOT: &str = "SELECT rootpage FROM sqlite_schema WHERE name='anchors_email'";
        const INSERT: &str = "INSERT INTO thread_anchors(account,message_id,email_id,changed) VALUES(?1,'second@example.test',?2,?3)";
        const PRIMARY: &str = "SELECT count(*) FROM thread_anchors NOT INDEXED";
        const INDEX: &str = "SELECT count(*) FROM thread_anchors INDEXED BY anchors_email";
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
        let blob = BlobId::from_bytes([4; 16]);
        let thread = ThreadId::from_bytes([5; 16]);
        let email = EmailId::from_bytes([6; 16]);
        let blob_bytes = encode(Row::Blob(body_row(b"", BlobKind::Message)));
        let thread_bytes = encode(Row::Thread);
        let email_bytes = encode(Row::Email(EmailRow {
            blob,
            thread,
            received_at: 0,
            origin: EmailOrigin::Jmap,
        }));
        let mut anchor = [0; 1024];
        let length = Key::ThreadAnchor("first@example.test", email)
            .encode(&mut anchor)
            .unwrap();
        let mut source = b"".as_slice();
        store
            .commit(
                &td_crypto::Provider,
                request(0),
                &[
                    Operation::put(Table::Blobs, blob.as_bytes(), &blob_bytes).unwrap(),
                    Operation::put(Table::Threads, thread.as_bytes(), &thread_bytes).unwrap(),
                    Operation::put(Table::Emails, email.as_bytes(), &email_bytes).unwrap(),
                    Operation::put(Table::ThreadAnchors, &anchor[..length], &[]).unwrap(),
                ],
                &mut [BlobSource {
                    id: blob,
                    source: &mut source,
                }],
            )
            .unwrap();
        assert_eq!(store.validate_integrity(deadline()), Ok(()));
        let page: u64 = {
            let writer = lock(&store.writer).unwrap();
            writer
                .native
                .run(|db| {
                    db.execute(
                        INSERT,
                        params![
                            ACCOUNT.as_bytes().as_slice(),
                            email.as_bytes().as_slice(),
                            1u64.to_be_bytes().as_slice()
                        ],
                    )
                    .map_err(sql)?;
                    let primary: i64 = db.query_row(PRIMARY, [], |row| row.get(0)).map_err(sql)?;
                    let indexed: i64 = db.query_row(INDEX, [], |row| row.get(0)).map_err(sql)?;
                    assert_eq!((primary, indexed), (2, 2));
                    let page: i64 = db.query_row(ROOT, [], |row| row.get(0)).map_err(sql)?;
                    Ok(u64::try_from(page).unwrap())
                })
                .unwrap()
        };
        assert_eq!(
            store.validate_integrity(deadline()),
            Err(ports::Error::Corrupt)
        );
        store.checkpoint(deadline()).unwrap();
        drop(store);
        // All SQLite connections are closed before this second DB descriptor.
        let path = db_path(&root, RootEntry::Database).unwrap();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        let offset = page
            .checked_sub(1)
            .unwrap()
            .checked_mul(PAGE_BYTES)
            .unwrap();
        let mut bytes = vec![0; usize::try_from(PAGE_BYTES).unwrap()];
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes[0], 0x0a, "fixture index root must be a leaf");
        assert_eq!(u16::from_be_bytes([bytes[3], bytes[4]]), 2);
        let cell = usize::from(u16::from_be_bytes([bytes[10], bytes[11]]));
        // This small fixture has one-byte payload and record-header varints.
        assert!(bytes[cell] < 128);
        assert_eq!(&bytes[cell + 1..cell + 4], &[4, 44, 44]);
        assert!(bytes[cell + 4] < 128);
        let account_start = cell + 5;
        assert_eq!(
            &bytes[account_start..account_start + 16],
            ACCOUNT.as_bytes()
        );
        let email_start = account_start + 16;
        assert_eq!(&bytes[email_start..email_start + 16], email.as_bytes());
        // Keep both entries and their ordering, but detach the second key.
        bytes[email_start..email_start + 16].fill(7);
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
        drop(file);
        let store = IndexStore::open(&mut root, clock, 1, deadline()).unwrap();
        {
            let writer = lock(&store.writer).unwrap();
            writer
                .native
                .run(|db| {
                    let primary: i64 = db.query_row(PRIMARY, [], |row| row.get(0)).map_err(sql)?;
                    let indexed: i64 = db.query_row(INDEX, [], |row| row.get(0)).map_err(sql)?;
                    assert_eq!((primary, indexed), (2, 2));
                    let quick: String = db
                        .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
                        .map_err(sql)?;
                    assert_eq!(quick, "ok");
                    let full: String = db
                        .query_row("PRAGMA integrity_check(1)", [], |row| row.get(0))
                        .map_err(sql)?;
                    assert_ne!(full, "ok");
                    assert!(full.contains("anchors_email"), "{full}");
                    let mut statement = db.prepare("PRAGMA foreign_key_check").map_err(sql)?;
                    assert!(statement
                        .query([])
                        .map_err(sql)?
                        .next()
                        .map_err(sql)?
                        .is_none());
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(
            store.validate_integrity(deadline()),
            Err(ports::Error::Corrupt)
        );
    }

    #[test]
    fn anchor_maintenance_uses_ordered_index_and_original_work_scope() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        for fault in ["none", "deadline", "reversal", "fuel", "denied"] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let clock = Arc::new(Timer(AtomicU64::new(1)));
            let store = IndexStore::create(
                &mut root,
                StoreEpoch::from_bytes([9; 16]),
                clock.clone(),
                1,
                deadline(),
            )
            .unwrap();
            store.create_account(ACCOUNT, deadline()).unwrap();
            let (budget, observed) = {
                let writer = lock(&store.writer).unwrap();
                let plan: Vec<String> = writer
                    .native
                    .run(|db| {
                        let mut statement = db
                            .prepare(&format!("EXPLAIN QUERY PLAN {ANCHOR_CARDINALITY}"))
                            .map_err(sql)?;
                        let rows = statement.query_map([], |row| row.get(3)).map_err(sql)?;
                        rows.collect::<Result<_, _>>().map_err(sql)
                    })
                    .unwrap();
                assert!(
                    plan.iter()
                        .any(|step| step.contains("COVERING INDEX anchors_email")),
                    "{plan:?}"
                );
                assert!(
                    !plan.iter().any(|step| step.contains("TEMP B-TREE")),
                    "{plan:?}"
                );
                let budget = writer.native.budget.clone();
                let hook_budget = budget.clone();
                let observed = Arc::new(AtomicU64::new(0));
                let hook_observed = observed.clone();
                let hook_clock = clock.clone();
                lock(&writer.native.connection)
                    .unwrap()
                    .authorizer(Some(move |context: AuthContext<'_>| {
                        if matches!(
                            context.action,
                            AuthAction::Read {
                                table_name: "thread_anchors",
                                column_name: "email_id"
                            }
                        ) {
                            hook_observed.fetch_add(1, Ordering::Relaxed);
                            match fault {
                                "deadline" => hook_clock.0.store(100, Ordering::Relaxed),
                                "reversal" => hook_clock.0.store(0, Ordering::Relaxed),
                                "fuel" => hook_budget.lock().unwrap().remaining = 0,
                                "denied" => return Authorization::Deny,
                                _ => (),
                            }
                        }
                        Authorization::Allow
                    }))
                    .unwrap();
                (budget, observed)
            };
            let expected = match fault {
                "none" => Ok(()),
                "deadline" => Err(ports::Error::Deadline),
                "reversal" => Err(ports::Error::Invalid),
                "fuel" => Err(ports::Error::Capacity),
                "denied" => Err(ports::Error::Io {
                    kind: std::io::ErrorKind::Other,
                    os_code: None,
                }),
                _ => panic!("unknown fault"),
            };
            assert_eq!(store.validate_integrity(deadline()), expected, "{fault}");
            assert!(observed.load(Ordering::Relaxed) > 0, "{fault}");
            {
                let writer = lock(&store.writer).unwrap();
                assert!(!writer.stopped);
                lock(&writer.native.connection)
                    .unwrap()
                    .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                    .unwrap();
            }
            clock.0.store(1, Ordering::Relaxed);
            if matches!(fault, "deadline" | "reversal" | "fuel") {
                assert_eq!(lock(&budget).unwrap().failure, expected.err(), "{fault}");
            }
            assert_eq!(store.validate_integrity(deadline()), Ok(()), "{fault}");
        }
    }

    #[test]
    fn integrity_maintenance_rejects_duplicate_anchors_across_all_accounts() {
        const INSERT: &str =
            "INSERT INTO thread_anchors(account,message_id,email_id,changed) VALUES(?1,?2,?3,?4)";
        let other = AccountId::from_bytes([11; 16]);
        let blob = BlobId::from_bytes([4; 16]);
        let thread = ThreadId::from_bytes([5; 16]);
        let email = EmailId::from_bytes([6; 16]);
        let sibling = EmailId::from_bytes([7; 16]);
        fn anchor(message: &str, email: EmailId) -> Vec<u8> {
            let mut bytes = vec![0; 1024];
            let n = Key::ThreadAnchor(message, email)
                .encode(&mut bytes)
                .unwrap();
            bytes.truncate(n);
            bytes
        }
        let mut unexpected = Vec::new();
        for damaged in [ACCOUNT, other] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let clock: Arc<dyn Clock> = Arc::new(Timer(AtomicU64::new(1)));
            let store = IndexStore::create(
                &mut root,
                StoreEpoch::from_bytes([9; 16]),
                clock.clone(),
                2,
                deadline(),
            )
            .unwrap();
            let blob_bytes = encode(Row::Blob(BlobRow {
                kind: BlobKind::Message,
                length: 0,
                digest: td_crypto::Provider.sha256().unwrap().finish().unwrap(),
                created_at: 0,
            }));
            let email_bytes = encode(Row::Email(EmailRow {
                blob,
                thread,
                received_at: 0,
                origin: EmailOrigin::Jmap,
            }));
            let thread_bytes = encode(Row::Thread);
            let first = anchor("first@example.test", email);
            let duplicate_id = anchor("first@example.test", sibling);
            let second = anchor("second@example.test", email);
            for account in [ACCOUNT, other] {
                store.create_account(account, deadline()).unwrap();
                let mut operations = vec![
                    Operation::put(Table::Blobs, blob.as_bytes(), &blob_bytes).unwrap(),
                    Operation::put(Table::Threads, thread.as_bytes(), &thread_bytes).unwrap(),
                    Operation::put(Table::Emails, email.as_bytes(), &email_bytes).unwrap(),
                ];
                if account == ACCOUNT {
                    operations.extend([
                        Operation::put(Table::Emails, sibling.as_bytes(), &email_bytes).unwrap(),
                        Operation::put(Table::ThreadAnchors, &first, &[]).unwrap(),
                        Operation::put(Table::ThreadAnchors, &duplicate_id, &[]).unwrap(),
                    ]);
                } else {
                    operations.push(Operation::put(Table::ThreadAnchors, &second, &[]).unwrap());
                }
                let mut empty = b"".as_slice();
                assert_eq!(
                    store.commit(
                        &td_crypto::Provider,
                        CommitRequest {
                            account,
                            epoch: StoreEpoch::from_bytes([9; 16]),
                            expected: Sequence::default(),
                            deadline: deadline(),
                            utc_ms: 0,
                        },
                        &operations,
                        &mut [BlobSource {
                            id: blob,
                            source: &mut empty
                        }]
                    ),
                    Ok(Sequence::from_u64(1))
                );
            }
            assert_eq!(store.validate_integrity(deadline()), Ok(()));
            let mut retained = store.view(damaged, deadline()).unwrap();
            {
                let writer = lock(&store.writer).unwrap();
                writer
                    .native
                    .run(|db| {
                        db.execute(
                            INSERT,
                            params![
                                damaged.as_bytes().as_slice(),
                                "extra@example.test",
                                email.as_bytes().as_slice(),
                                1u64.to_be_bytes().as_slice()
                            ],
                        )
                        .map_err(sql)?;
                        let physical: String = db
                            .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
                            .map_err(sql)?;
                        assert_eq!(physical, "ok");
                        let mut statement = db.prepare("PRAGMA foreign_key_check").map_err(sql)?;
                        assert!(statement
                            .query([])
                            .map_err(sql)?
                            .next()
                            .map_err(sql)?
                            .is_none());
                        Ok(())
                    })
                    .unwrap();
            }
            let actual = store.validate_integrity(deadline());
            if actual != Err(ports::Error::Corrupt) {
                unexpected.push((damaged, actual));
            }
            assert!(retained
                .get(Key::ThreadAnchor("extra@example.test", email), &mut [0; 64])
                .unwrap()
                .is_none());
            drop(retained);
            // Startup does not make the new maintenance scan implicit.
            drop(store);
            let store = IndexStore::open(&mut root, clock, 1, deadline()).unwrap();
            let mut fresh = store.view(damaged, deadline()).unwrap();
            assert!(fresh
                .get(Key::ThreadAnchor("extra@example.test", email), &mut [0; 64])
                .unwrap()
                .is_some());
            drop(fresh);
            let actual = store.validate_integrity(deadline());
            if actual != Err(ports::Error::Corrupt) {
                unexpected.push((damaged, actual));
            }
        }
        assert!(
            unexpected.is_empty(),
            "duplicate anchor maintenance accepted: {unexpected:?}"
        );
    }

    #[test]
    fn native_thread_anchors_allow_only_one_final_anchor_per_email() {
        fn apply(
            store: &IndexStore<'_>,
            account: AccountId,
            expected: u64,
            operations: &[Operation<'_>],
            sources: &mut [BlobSource<'_>],
            encoded: bool,
        ) -> Result<Sequence, CommitError> {
            let request = CommitRequest {
                account,
                epoch: StoreEpoch::from_bytes([9; 16]),
                expected: Sequence::from_u64(expected),
                utc_ms: 0,
                deadline: deadline(),
            };
            if encoded {
                commit_encoded_rows(store, request, operations, sources)
            } else {
                store.commit(&td_crypto::Provider, request, operations, sources)
            }
        }
        fn anchor_key(name: &str, email: EmailId) -> Vec<u8> {
            let mut bytes = vec![0; 1024];
            let length = Key::ThreadAnchor(name, email).encode(&mut bytes).unwrap();
            bytes.truncate(length);
            bytes
        }
        #[derive(Clone, Copy, Debug)]
        enum Form {
            Both,
            Reverse,
            OnlyNew,
        }
        let mut unexpected = Vec::new();
        for encoded in [false, true] {
            for existing in [false, true] {
                for form in [Form::Both, Form::Reverse, Form::OnlyNew] {
                    if matches!(form, Form::OnlyNew) && !existing {
                        continue;
                    }
                    let reverse = matches!(form, Form::Reverse);
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
                    let other = AccountId::from_bytes([11; 16]);
                    let blob = BlobId::from_bytes([4; 16]);
                    let fresh = BlobId::from_bytes([8; 16]);
                    let thread = ThreadId::from_bytes([5; 16]);
                    let email = EmailId::from_bytes([6; 16]);
                    let row = EmailRow {
                        blob,
                        thread,
                        received_at: 0,
                        origin: EmailOrigin::Jmap,
                    };
                    let email_bytes = encode(Row::Email(row));
                    let thread_bytes = encode(Row::Thread);
                    let blob_bytes = encode(Row::Blob(BlobRow {
                        kind: BlobKind::Message,
                        length: 0,
                        digest: td_crypto::Provider.sha256().unwrap().finish().unwrap(),
                        created_at: 0,
                    }));
                    let first = anchor_key("first@example.test", email);
                    let second = anchor_key("second@example.test", email);
                    let put_first = Operation::put(Table::ThreadAnchors, &first, &[]).unwrap();
                    let put_second = Operation::put(Table::ThreadAnchors, &second, &[]).unwrap();
                    let delete_first = Operation::delete(Table::ThreadAnchors, &first).unwrap();
                    let delete_second = Operation::delete(Table::ThreadAnchors, &second).unwrap();
                    for account in [ACCOUNT, other] {
                        store.create_account(account, deadline()).unwrap();
                        let mut setup = vec![
                            Operation::put(Table::Blobs, blob.as_bytes(), &blob_bytes).unwrap(),
                            Operation::put(Table::Threads, thread.as_bytes(), &thread_bytes)
                                .unwrap(),
                            Operation::put(Table::Emails, email.as_bytes(), &email_bytes).unwrap(),
                        ];
                        if account == other {
                            setup.push(put_second);
                        } else if existing {
                            setup.push(put_first);
                        }
                        assert_eq!(
                            apply(
                                &store,
                                account,
                                0,
                                &setup,
                                &mut [BlobSource {
                                    id: blob,
                                    source: &mut b"".as_slice()
                                }],
                                encoded
                            ),
                            Ok(Sequence::from_u64(1))
                        );
                    }
                    lock(&store.writer)
                        .unwrap()
                        .native
                        .run(|db| {
                            let mut statement = db
                                .prepare(&format!("EXPLAIN QUERY PLAN {SECOND_ANCHOR}"))
                                .map_err(sql)?;
                            let details = statement
                                .query_map(
                                    params![
                                        ACCOUNT.as_bytes().as_slice(),
                                        email.as_bytes().as_slice()
                                    ],
                                    |row| row.get::<_, String>(3),
                                )
                                .map_err(sql)?
                                .collect::<Result<Vec<_>, _>>()
                                .map_err(sql)?;
                            assert!(
                                details
                                    .iter()
                                    .any(|detail| detail.contains(
                                        "SEARCH thread_anchors USING COVERING INDEX anchors_email"
                                    ) && detail.contains("account=? AND email_id=?")),
                                "{details:?}"
                            );
                            Ok(())
                        })
                        .unwrap();
                    let body = b"fresh body";
                    let mut hash = td_crypto::Provider.sha256().unwrap();
                    hash.update(body).unwrap();
                    let fresh_bytes = encode(Row::Blob(BlobRow {
                        kind: BlobKind::Message,
                        length: body.len() as u64,
                        digest: hash.finish().unwrap(),
                        created_at: 0,
                    }));
                    let fresh_put =
                        Operation::put(Table::Blobs, fresh.as_bytes(), &fresh_bytes).unwrap();
                    let operations = match form {
                        Form::Both => vec![fresh_put, put_first, put_second],
                        Form::Reverse => vec![fresh_put, put_second, put_first],
                        Form::OnlyNew => vec![fresh_put, put_second],
                    };
                    let mut old = store.view(ACCOUNT, deadline()).unwrap();
                    let mut input = body.as_slice();
                    let result = apply(
                        &store,
                        ACCOUNT,
                        1,
                        &operations,
                        &mut [BlobSource {
                            id: fresh,
                            source: &mut input,
                        }],
                        encoded,
                    );
                    if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                        unexpected.push((encoded, existing, form, result));
                        continue;
                    }
                    let mut now = store.view(ACCOUNT, deadline()).unwrap();
                    assert_eq!(now.identity().committed_sequence, Sequence::from_u64(1));
                    assert_eq!(
                        now.get(Key::ThreadAnchor("first@example.test", email), &mut [0; 64])
                            .unwrap(),
                        existing.then_some((Row::ThreadAnchor, Sequence::from_u64(1)))
                    );
                    assert!(now
                        .get(
                            Key::ThreadAnchor("second@example.test", email),
                            &mut [0; 64]
                        )
                        .unwrap()
                        .is_none());
                    assert!(now.get(Key::Blob(fresh), &mut [0; 128]).unwrap().is_none());
                    drop(now);
                    // Final-state refusal can consume the prepared source; retry uses a fresh reader.
                    let mut input = body.as_slice();
                    assert_eq!(
                        apply(
                            &store,
                            ACCOUNT,
                            1,
                            &[fresh_put, put_first, put_second, delete_first],
                            &mut [BlobSource {
                                id: fresh,
                                source: &mut input
                            }],
                            encoded
                        ),
                        Ok(Sequence::from_u64(2))
                    );
                    let replace = if reverse {
                        [delete_second, put_first]
                    } else {
                        [put_first, delete_second]
                    };
                    assert_eq!(
                        apply(&store, ACCOUNT, 2, &replace, &mut [], encoded),
                        Ok(Sequence::from_u64(3))
                    );
                    assert_eq!(
                        apply(
                            &store,
                            ACCOUNT,
                            3,
                            &[put_first, put_first],
                            &mut [],
                            encoded
                        ),
                        Ok(Sequence::from_u64(4))
                    );
                    // A duplicated Message-ID across different Emails remains valid.
                    let new_email = EmailId::from_bytes([10; 16]);
                    let duplicate = anchor_key("first@example.test", new_email);
                    assert_eq!(
                        apply(
                            &store,
                            ACCOUNT,
                            4,
                            &[
                                Operation::put(Table::ThreadAnchors, &duplicate, &[]).unwrap(),
                                Operation::put(Table::Emails, new_email.as_bytes(), &email_bytes)
                                    .unwrap(),
                            ],
                            &mut [],
                            encoded
                        ),
                        Ok(Sequence::from_u64(5))
                    );
                    let mut now = store.view(ACCOUNT, deadline()).unwrap();
                    assert_eq!(
                        now.get(Key::ThreadAnchor("first@example.test", email), &mut [0; 64])
                            .unwrap(),
                        Some((Row::ThreadAnchor, Sequence::from_u64(4)))
                    );
                    assert_eq!(
                        now.get(
                            Key::ThreadAnchor("first@example.test", new_email),
                            &mut [0; 64]
                        )
                        .unwrap(),
                        Some((Row::ThreadAnchor, Sequence::from_u64(5)))
                    );
                    assert!(now
                        .get(
                            Key::ThreadAnchor("second@example.test", email),
                            &mut [0; 64]
                        )
                        .unwrap()
                        .is_none());
                    assert_eq!(
                        old.get(Key::ThreadAnchor("first@example.test", email), &mut [0; 64])
                            .unwrap(),
                        existing.then_some((Row::ThreadAnchor, Sequence::from_u64(1)))
                    );
                    drop(now);
                    let mut other_view = store.view(other, deadline()).unwrap();
                    assert_eq!(
                        other_view
                            .get(
                                Key::ThreadAnchor("second@example.test", email),
                                &mut [0; 64]
                            )
                            .unwrap(),
                        Some((Row::ThreadAnchor, Sequence::from_u64(1)))
                    );
                }
            }
        }
        assert!(
            unexpected.is_empty(),
            "multiple anchors committed: {unexpected:?}"
        );
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
            epoch: StoreEpoch::from_bytes([9; 16]),
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
            epoch: StoreEpoch::from_bytes([9; 16]),
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
                    epoch: StoreEpoch::from_bytes([9; 16]),
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
            epoch: StoreEpoch::from_bytes([9; 16]),
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
            epoch: StoreEpoch::from_bytes([9; 16]),
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
        assert_eq!(
            store.usage_fence(deadline()).err(),
            Some(ports::Error::WriterStopped)
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
