//! Passive logical charges captured from one account snapshot.
use super::*;

/// Snapshot totals, not a quota reservation or proof of physical space usage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogicalUsage {
    pub identity: ViewIdentity,
    pub body_bytes: u64,
    pub blob_count: u64,
    pub upload_bytes: u64,
    pub queue_bytes: u64,
    pub queue_submissions: u64,
}

const BODIES: &str = "SELECT coalesce(sum(b.length),0),count(*),
    coalesce(sum(CASE WHEN EXISTS(SELECT 1 FROM leases l
        WHERE l.account=b.account AND l.blob_id=b.id) THEN b.length ELSE 0 END),0),
    coalesce(sum(CASE WHEN EXISTS(SELECT 1 FROM submissions s
        WHERE s.account=b.account AND s.transmitted_blob_id=b.id)
        THEN b.length ELSE 0 END),0)
    FROM blobs b WHERE b.account=?1";
const SUBMISSIONS: &str = "SELECT count(*) FROM submissions WHERE account=?1";

impl IndexReadView<'_, '_> {
    /// Read logical body/category charges without renewing the snapshot's scope.
    /// Expiry and completion do not remove retained lease/submission charges.
    pub fn logical_usage(&mut self) -> Result<LogicalUsage, ports::Error> {
        let identity = self.identity;
        self.read_snapshot(|native| native.run(|db| read_account(db, identity)))
    }
}

pub(super) fn read_account(
    db: &Connection,
    identity: ViewIdentity,
) -> Result<LogicalUsage, ports::Error> {
    let (body_bytes, blob_count, upload_bytes, queue_bytes): (i64, i64, i64, i64) = db
        .query_row(
            BODIES,
            params![identity.account.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(sql)?;
    let queue_submissions: i64 = db
        .query_row(
            SUBMISSIONS,
            params![identity.account.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .map_err(sql)?;
    Ok(LogicalUsage {
        identity,
        body_bytes: u64::try_from(body_bytes).map_err(|_| ports::Error::Corrupt)?,
        blob_count: u64::try_from(blob_count).map_err(|_| ports::Error::Corrupt)?,
        upload_bytes: u64::try_from(upload_bytes).map_err(|_| ports::Error::Corrupt)?,
        queue_bytes: u64::try_from(queue_bytes).map_err(|_| ports::Error::Corrupt)?,
        queue_submissions: u64::try_from(queue_submissions).map_err(|_| ports::Error::Corrupt)?,
    })
}

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;
