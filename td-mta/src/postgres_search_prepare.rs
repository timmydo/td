//! Finite owner-side preparation of missing search evidence across accounts.
pub use crate::postgres_blob_scrub::Key;
use crate::{
    ids::{AccountId, BlobId, StoreEpoch},
    postgres_body,
    postgres_probe::{self, Request},
    postgres_schema, postgres_search,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
use td_crypto::TrustStore;
use tokio_postgres::types::Type;

pub const VERSION: u32 = postgres_search::VERSION as u32;
pub const MAXIMUM: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Body(postgres_body::Error),
    Owner,
    Invalid,
    Database,
    Indeterminate,
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
    pub written: usize,
    pub skipped: usize,
    pub verified_bytes: u64,
    pub done: bool,
}
fn key(row: &tokio_postgres::Row, account: usize, blob: usize) -> Result<Key, Error> {
    let account: &[u8] = row.try_get(account).map_err(|_| Error::Invalid)?;
    let blob: &[u8] = row.try_get(blob).map_err(|_| Error::Invalid)?;
    Ok(Key {
        account: AccountId::from_bytes(account.try_into().map_err(|_| Error::Invalid)?),
        blob: BlobId::from_bytes(blob.try_into().map_err(|_| Error::Invalid)?),
    })
}

/// The continuation follows known COMMIT. Compatible evidence is not audited.
pub fn run(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    options: Options,
) -> Result<Report, Error> {
    if request.mode != postgres_probe::Mode::Preparation {
        return Err(Error::Owner);
    }
    if !(1..=MAXIMUM).contains(&options.maximum)
        || options
            .after
            .is_some_and(|after| options.through.is_none_or(|end| after > end))
    {
        return Err(Error::Invalid);
    }
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_search_backfill_worker(request, trust, deadline, |worker| {
        let (rows, through) = worker.run(move |client| Box::pin(async move {
            crate::postgres_maintenance::owner(client).await.map_err(|_| Error::Owner)?;
            let receipt = postgres_schema::lock_and_inspect(client).await.map_err(Error::Schema)?;
            if receipt.store_epoch != epoch { return Err(Error::Invalid); }
            client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ; SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_| Error::Database)?;
            let through = match options.through {
                Some(end) => Some(end),
                None => client.query_typed_opt("SELECT account,id FROM td_mta.blobs ORDER BY account DESC,id DESC LIMIT 1", &[]).await.map_err(|_| Error::Database)?.as_ref().map(|row| key(row,0,1)).transpose()?,
            };
            let Some(end) = through else { return Ok((Vec::new(), None)); };
            let after_account = options.after.map(|key| *key.account.as_bytes());
            let after_blob = options.after.map(|key| *key.blob.as_bytes());
            let rows = client.query_typed("SELECT a.sequence,a.floor,b.length,b.digest,b.created_at,b.chunk_count,b.changed,(SELECT count(*) FROM td_mta.blob_chunks c WHERE c.account=b.account AND c.blob_id=b.id),b.account,b.id FROM td_mta.blobs b JOIN td_mta.accounts a ON a.id=b.account WHERE ($1::bytea IS NULL OR (b.account,b.id)>($1,$2)) AND (b.account,b.id)<=($3,$4) AND EXISTS(SELECT 1 FROM td_mta.emails e WHERE e.account=b.account AND e.blob_id=b.id) AND NOT EXISTS(SELECT 1 FROM td_mta.blob_search s WHERE s.account=b.account AND s.blob_id=b.id AND s.version=$5 AND s.body_length=b.length AND s.body_digest=b.digest AND s.candidate_text IS NOT NULL) ORDER BY b.account,b.id LIMIT $6", &[
                (&after_account.as_ref().map(|v| v.as_slice()), Type::BYTEA),
                (&after_blob.as_ref().map(|v| v.as_slice()), Type::BYTEA),
                (&end.account.as_bytes().as_slice(), Type::BYTEA),
                (&end.blob.as_bytes().as_slice(), Type::BYTEA),
                (&postgres_search::VERSION, Type::INT4),
                (&((options.maximum+1) as i64), Type::INT8),
            ]).await.map_err(|_| Error::Database)?;
            Ok((rows, through))
        })).map_err(Error::Connection)??;
        let mut report = Report {
            after: options.after,
            through,
            scanned: 0,
            written: 0,
            skipped: 0,
            verified_bytes: 0,
            done: rows.len() <= options.maximum,
        };
        let mut encoded_bytes = 0usize;
        for row in rows.iter().take(options.maximum) {
            let id = key(row, 8, 9)?;
            let metadata = postgres_body::blob_metadata(
                row,
                id.account,
                epoch,
                id.blob,
                postgres_body::MAX_BODY_BYTES,
            )
            .map_err(Error::Body)?;
            if report.verified_bytes + metadata.body.length > postgres_body::MAX_BODY_BYTES {
                report.done = false;
                break;
            }
            let value = postgres_body::with_interpreted_pin(worker, metadata, deadline, |body| {
                postgres_search::prepare(body, deadline)
            })
            .map_err(Error::Body)?;
            report.verified_bytes += metadata.body.length;
            let size = value
                .as_ref()
                .map_or(0, |v| v.fields.len() + v.candidate_text.len());
            if encoded_bytes + size > 2 * postgres_search::MAX_BYTES {
                report.done = false;
                break;
            }
            encoded_bytes += size;
            let written = worker
                .run(move |client| Box::pin(postgres_search::store(client, metadata, value)))
                .map_err(Error::Connection)?
                .map_err(|_| Error::Database)?;
            if written {
                report.written += 1;
            } else {
                report.skipped += 1;
            }
            report.scanned += 1;
            report.after = Some(id);
        }
        if Instant::now() >= deadline {
            return Err(Error::Connection(postgres_probe::Error::Deadline));
        }
        marker.store(true, Ordering::Relaxed);
        worker
            .run(|client| Box::pin(async move { client.batch_execute("COMMIT").await }))
            .map_err(|_| Error::Indeterminate)?
            .map_err(|_| Error::Indeterminate)?;
        Ok(report)
    });
    match result {
        Ok(result) => result,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}
