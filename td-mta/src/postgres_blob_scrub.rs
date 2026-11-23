//! Finite owner-side body verification pages; never repairs stored mail.
use crate::{
    ids::{AccountId, BlobId, StoreEpoch},
    ports, postgres_body,
    postgres_probe::{self, Request},
    postgres_schema,
};
use std::time::Instant;
use td_crypto::TrustStore;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Key {
    pub account: AccountId,
    pub blob: BlobId,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Body(postgres_body::Error),
    Owner,
    Invalid,
    Database,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Options {
    pub after: Option<Key>,
    pub through: Option<Key>,
    pub maximum: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Report {
    pub after: Option<Key>,
    pub through: Option<Key>,
    pub scanned: usize,
    pub failed: usize,
    pub first_failure: Option<Key>,
    pub verified_bytes: u64,
    pub done: bool,
}
fn key(row: &crate::postgres_probe::Row, account: usize, blob: usize) -> Result<Key, Error> {
    let account: &[u8] = row.try_get(account).map_err(|_| Error::Invalid)?;
    let blob: &[u8] = row.try_get(blob).map_err(|_| Error::Invalid)?;
    Ok(Key {
        account: AccountId::from_bytes(account.try_into().map_err(|_| Error::Invalid)?),
        blob: BlobId::from_bytes(blob.try_into().map_err(|_| Error::Invalid)?),
    })
}
fn corrupt(error: postgres_body::Error) -> bool {
    matches!(
        error,
        postgres_body::Error::Integrity | postgres_body::Error::Read(ports::Error::Corrupt)
    )
}
/// At most 256 bodies and 64 MiB per read-only primary snapshot. Continuations
/// carry the first page's upper key; later inserts behind the cursor belong to
/// the next cycle. A returned cursor follows a known completed ROLLBACK.
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    options: Options,
) -> Result<Report, Error> {
    if request.mode != postgres_probe::Mode::Preparation {
        return Err(Error::Owner);
    }
    if !(1..=256).contains(&options.maximum)
        || options
            .after
            .is_some_and(|after| options.through.is_none_or(|end| after > end))
    {
        return Err(Error::Invalid);
    }
    let result = postgres_probe::with_body_worker(request, trust, deadline, |worker| {
        let (rows, through) = worker.run(move |client| {
            crate::postgres_maintenance::owner(client).map_err(|_| Error::Owner)?;
            let receipt = postgres_schema::lock_and_inspect(client).map_err(Error::Schema)?;
            if receipt.store_epoch != epoch { return Err(Error::Invalid); }
            client.batch("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000").map_err(|_| Error::Database)?;
            let through = match options.through {
                Some(end) => Some(end),
                None => client.query_opt("SELECT account,id FROM td_mta.blobs ORDER BY account DESC,id DESC LIMIT 1", &[]).map_err(|_| Error::Database)?.as_ref().map(|row|key(&row.row(),0,1)).transpose()?,
            };
            let Some(end) = through else { return Ok((None, None)); };
            let after_account = options.after.map(|k| *k.account.as_bytes());
            let after_blob = options.after.map(|k| *k.blob.as_bytes());
            let rows = client.query("SELECT a.sequence,a.floor,b.length,b.digest,b.created_at,b.chunk_count,b.changed,(SELECT count(*) FROM td_mta.blob_chunks c WHERE c.account=b.account AND c.blob_id=b.id),b.account,b.id FROM td_mta.blobs b JOIN td_mta.accounts a ON a.id=b.account WHERE ($1::bytea IS NULL OR (b.account,b.id)>($1,$2)) AND (b.account,b.id)<=($3,$4) ORDER BY b.account,b.id LIMIT $5", &[&after_account.as_ref().map(|v|v.as_slice()), &after_blob.as_ref().map(|v|v.as_slice()), &end.account.as_bytes().as_slice(), &end.blob.as_bytes().as_slice(), &((options.maximum+1) as i64)]).map_err(|_|Error::Database)?;
            Ok((Some(rows), through))
        }).map_err(Error::Connection)??;
        let mut report = Report { after:options.after, through, scanned:0, failed:0, first_failure:None, verified_bytes:0, done: rows.as_ref().map_or(0, crate::postgres_probe::Rows::len) <= options.maximum };
        let mut attempted_bytes = 0u64;
        for row in rows.iter().flat_map(crate::postgres_probe::Rows::iter).take(options.maximum) {
            let id = key(&row,8,9)?;
            let metadata = postgres_body::blob_metadata(&row,id.account,epoch,id.blob,postgres_body::MAX_BODY_BYTES);
            let length = metadata.as_ref().map_or(0, |m| m.body.length);
            if attempted_bytes + length > postgres_body::MAX_BODY_BYTES {
                report.done = false;
                break;
            }
            attempted_bytes += length;
            let result = metadata.and_then(|metadata|postgres_body::with_verified_body(worker,metadata,deadline,|_|Ok(())));
            match result {
                Ok(()) => report.verified_bytes += length,
                Err(error) if corrupt(error) => {
                    report.failed += 1;
                    report.first_failure.get_or_insert(id);
                }
                Err(error) => return Err(Error::Body(error)),
            }
            report.scanned += 1;
            report.after = Some(id);
        }
        worker.run(|client|{client.rollback()}).map_err(Error::Connection)?.map_err(|_|Error::Database)?;
        if Instant::now() >= deadline { return Err(Error::Connection(postgres_probe::Error::Deadline)); }
        Ok(report)
    }).map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}
