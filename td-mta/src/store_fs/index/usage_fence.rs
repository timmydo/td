//! Keep cold logical reconciliation stable while its coordinator initializes.
use super::*;

/// Passive whole-store totals; database/WAL and pending effects are excluded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreLogicalUsage {
    pub epoch: StoreEpoch,
    pub accounts: u32,
    pub body_bytes: u64,
    pub blob_count: u64,
    pub upload_bytes: u64,
    pub queue_bytes: u64,
    pub queue_submissions: u64,
}
impl StoreLogicalUsage {
    fn add(&mut self, account: LogicalUsage) -> Result<(), ports::Error> {
        self.accounts = self
            .accounts
            .checked_add(1)
            .filter(|count| *count <= MAX_ACCOUNTS)
            .ok_or(ports::Error::Capacity)?;
        self.body_bytes = self
            .body_bytes
            .checked_add(account.body_bytes)
            .ok_or(ports::Error::Capacity)?;
        self.blob_count = self
            .blob_count
            .checked_add(account.blob_count)
            .ok_or(ports::Error::Capacity)?;
        self.upload_bytes = self
            .upload_bytes
            .checked_add(account.upload_bytes)
            .ok_or(ports::Error::Capacity)?;
        self.queue_bytes = self
            .queue_bytes
            .checked_add(account.queue_bytes)
            .ok_or(ports::Error::Capacity)?;
        self.queue_submissions = self
            .queue_submissions
            .checked_add(account.queue_submissions)
            .ok_or(ports::Error::Capacity)?;
        Ok(())
    }
}

/// Holds the writer fence after the read transaction ends. Drop releases it.
/// Existing views remain readable; new views, mutations and maintenance are Busy.
/// Copied totals are observations, not effect or quota authority.
#[must_use = "retain the writer fence until accounting initialization finishes"]
pub struct UsageFence<'s> {
    writer: MutexGuard<'s, Writer>,
    usage: StoreLogicalUsage,
    files: StoreFileUsage,
}
impl UsageFence<'_> {
    pub fn usage(&self) -> StoreLogicalUsage {
        self.usage
    }
}

impl IndexStore<'_> {
    /// Cold whole-store capture, preserving the writer fence for reconciliation.
    /// The future coordinator must also quiesce workers and reconcile pending effects.
    pub fn usage_fence(&self, deadline: Deadline) -> Result<UsageFence<'_>, ports::Error> {
        let (mut writer, acquired) = self.writer_observed(deadline)?;
        if writer.stopped {
            return Err(ports::Error::WriterStopped);
        }
        writer.native.begin_work_after(deadline, acquired)?;
        let result = writer.native.run(|db| {
            db.execute_batch("BEGIN DEFERRED").map_err(sql)?;
            let mut total = StoreLogicalUsage {
                epoch: self.epoch,
                accounts: 0,
                body_bytes: 0,
                blob_count: 0,
                upload_bytes: 0,
                queue_bytes: 0,
                queue_submissions: 0,
            };
            let mut accounts = db
                .prepare("SELECT id FROM accounts ORDER BY id")
                .map_err(sql)?;
            let mut rows = accounts.query([]).map_err(sql)?;
            while let Some(row) = rows.next().map_err(sql)? {
                writer.native.check()?;
                if total.accounts >= MAX_ACCOUNTS {
                    return Err(ports::Error::Capacity);
                }
                let account = AccountId::from_bytes(row.get(0).map_err(sql)?);
                let view_identity = identity(db, account, self.epoch)?;
                total.add(usage::read_account(db, view_identity)?)?;
            }
            Ok(total)
        });
        if !writer.native.rollback() {
            writer.stopped = true;
            return Err(ports::Error::WriterStopped);
        }
        let usage = result?;
        let files = StoreFileUsage::capture(self.root, &writer.native)?;
        Ok(UsageFence {
            writer,
            usage,
            files,
        })
    }
}

/// Verified logical file extents, including allocated/reusable pages and WAL tails.
/// They do not measure filesystem free space or allocated blocks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreFileUsage {
    pub database_bytes: u64,
    pub wal_bytes: u64,
}
impl StoreFileUsage {
    fn capture(root: &LockedRoot, native: &Native) -> Result<Self, ports::Error> {
        native.check()?;
        let database = fs::symlink_metadata(db_path(root, RootEntry::Database)?)?;
        let database_bytes = validated_file_length(root, &database, MAX_PAGES * PAGE_BYTES)?;
        native.check()?;
        let wal_bytes = match optional_metadata(&db_path(root, RootEntry::Wal)?)? {
            Some(metadata) => validated_file_length(root, &metadata, MAX_WAL_BYTES)?,
            None => 0,
        };
        native.check()?;
        Ok(Self {
            database_bytes,
            wal_bytes,
        })
    }
}

/// Trusted cold reconciliation of resources outside the authoritative database.
/// The caller must quiesce their owners and settle all pending effects first.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuxiliaryUsage {
    pub sort_bytes: u64,
    pub response_bytes: u64,
    pub cache_bytes: u64,
    pub log_bytes: u64,
    pub cold_bytes: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LedgerInitError {
    Store(ports::Error),
    Ledger(crate::admission::logical::Error),
}
impl std::fmt::Display for LedgerInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => error.fmt(f),
            Self::Ledger(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for LedgerInitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::Ledger(error) => Some(error),
        }
    }
}
impl UsageFence<'_> {
    pub fn file_usage(&self) -> StoreFileUsage {
        self.files
    }
    /// Consume this cold capture to initialize the existing ledger, then release
    /// the writer fence. No reservation or effect ticket is fabricated.
    pub fn initialize_leases<'a>(
        self,
        plan: &crate::admission::Plan,
        auxiliary: AuxiliaryUsage,
        states: &'a mut [crate::ownership::SlotState],
        cells: &'a mut [crate::admission::logical::Cell],
    ) -> Result<crate::admission::logical::Leases<'a>, LedgerInitError> {
        use crate::admission::{
            logical,
            quota::{Kind, Usage},
        };
        self.writer.native.check().map_err(LedgerInitError::Store)?;
        let mut used = Usage::default();
        for (kind, amount) in [
            (Kind::BodyBytes, self.usage.body_bytes),
            (Kind::BlobCount, self.usage.blob_count),
            (Kind::UploadBytes, self.usage.upload_bytes),
            (Kind::QueueBytes, self.usage.queue_bytes),
            (Kind::QueueSubmissions, self.usage.queue_submissions),
            (Kind::DatabaseBytes, self.files.database_bytes),
            (Kind::WalBytes, self.files.wal_bytes),
            (Kind::SortBytes, auxiliary.sort_bytes),
            (Kind::ResponseBytes, auxiliary.response_bytes),
            (Kind::CacheBytes, auxiliary.cache_bytes),
            (Kind::LogBytes, auxiliary.log_bytes),
            (Kind::ColdBytes, auxiliary.cold_bytes),
        ] {
            used.add(kind, amount)
                .map_err(|error| LedgerInitError::Ledger(error.into()))?;
        }
        let ledger =
            logical::Leases::new(plan, used, states, cells).map_err(LedgerInitError::Ledger)?;
        self.writer.native.check().map_err(LedgerInitError::Store)?;
        Ok(ledger)
    }
}

#[cfg(test)]
#[path = "initialization_tests.rs"]
mod tests;
