//! Historical change-row validation, not event completeness or restore authority.
use crate::{
    format::Sequence,
    ids::{AccountId, StoreEpoch},
    ports::{ChangeCursor, ViewIdentity},
    postgres_change_row,
    postgres_probe::{self, Request},
    postgres_schema,
};
use std::time::Instant;
use td_crypto::TrustStore;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Epoch,
    Missing,
    Integrity,
    Capacity,
    Database,
}

/// Includes physically retained rows at or below the captured history floor.
/// No claim about missing events, live objects, receipts or serving authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Report {
    identity: ViewIdentity,
    rows: u64,
    retired_rows: u64,
}
impl Report {
    pub const fn identity(self) -> ViewIdentity {
        self.identity
    }
    pub const fn rows(self) -> u64 {
        self.rows
    }
    pub const fn retired_rows(self) -> u64 {
        self.retired_rows
    }
}

/// Check every physically retained change row in one finite primary snapshot.
/// The caller supplies administrative connection access and a total row bound.
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    account: AccountId,
    max_rows: u64,
) -> Result<Report, Error> {
    let result = postgres_probe::with_body_worker(request, trust, deadline, |worker| {
        let identity = worker.run(move |client| Box::pin(async move {
            let schema = postgres_schema::lock_and_inspect(client).await.map_err(Error::Schema)?;
            if schema.store_epoch != epoch { return Err(Error::Epoch); }
            client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_| Error::Database)?;
            let rows = client.query("SELECT sequence,floor FROM td_mta.accounts WHERE id=$1 LIMIT 2", &[&account.as_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
            if rows.is_empty() { return Err(Error::Missing); }
            if rows.len() != 1 { return Err(Error::Integrity); }
            let row = rows.first().ok_or(Error::Integrity)?;
            let sequence = |index| -> Result<Sequence, Error> {
                let bytes: &[u8] = row.try_get(index).map_err(|_| Error::Integrity)?;
                Ok(Sequence::from_u64(u64::from_be_bytes(bytes.try_into().map_err(|_| Error::Integrity)?)))
            };
            let committed_sequence = sequence(0)?;
            let history_floor = sequence(1)?;
            if history_floor > committed_sequence { return Err(Error::Integrity); }
            Ok(ViewIdentity { account, epoch, committed_sequence, history_floor })
        })).map_err(Error::Connection)??;
        let mut report = Report { identity, rows: 0, retired_rows: 0 };
        let mut after: Option<ChangeCursor> = None;
        loop {
            let rows = worker.run(move |client| Box::pin(async move {
                let sequence = after.map(|cursor| cursor.sequence.number().to_be_bytes());
                let sequence = sequence.as_ref().map(|bytes| bytes.as_slice());
                let operation = after.map_or(0, |cursor| i64::from(cursor.operation));
                client.query("SELECT sequence,operation,action,object,kind FROM td_mta.changes WHERE account=$1 AND ($2::bytea IS NULL OR (sequence,operation)>($2,$3)) ORDER BY sequence,operation LIMIT 128", &[&account.as_bytes().as_slice(), &sequence, &operation]).await
            })).map_err(Error::Connection)?.map_err(|_| Error::Database)?;
            if rows.len() > 128 { return Err(Error::Integrity); }
            for row in &rows {
                let count = report.rows.checked_add(1).ok_or(Error::Capacity)?;
                if count > max_rows { return Err(Error::Capacity); }
                let change = postgres_change_row::read(row).map_err(|_| Error::Integrity)?;
                let cursor = change.cursor;
                if cursor.sequence > identity.committed_sequence
                    || after.is_some_and(|prior| (cursor.sequence, cursor.operation) <= (prior.sequence, prior.operation)) {
                    return Err(Error::Integrity);
                }
                if cursor.sequence <= identity.history_floor {
                    report.retired_rows = report.retired_rows.checked_add(1).ok_or(Error::Capacity)?;
                }
                report.rows = count;
                after = Some(cursor);
            }
            if rows.len() < 128 { break; }
        }
        worker.run(|client| Box::pin(async move { client.batch_execute("ROLLBACK").await })).map_err(Error::Connection)?.map_err(|_| Error::Database)?;
        Ok(report)
    }).map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}
