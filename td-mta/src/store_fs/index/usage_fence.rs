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
    _writer: MutexGuard<'s, Writer>,
    usage: StoreLogicalUsage,
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
        let mut writer = self.writer(deadline)?;
        if writer.stopped {
            return Err(ports::Error::WriterStopped);
        }
        writer.native.begin_work(deadline)?;
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
        writer.native.check()?;
        Ok(UsageFence {
            _writer: writer,
            usage,
        })
    }
}
