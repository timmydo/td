//! Bounded, resumable owner-side rebuild of derived immutable header values.
use crate::{
    ids::{AccountId, BlobId, StoreEpoch},
    postgres_body,
    postgres_probe::{self, Request},
    postgres_schema, postgres_summaries,
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
pub struct Report {
    pub scanned: usize,
    pub written: usize,
    pub skipped: usize,
    pub structures_written: usize,
    pub structures_skipped: usize,
    pub verified_bytes: u64,
    pub after: Option<BlobId>,
    pub done: bool,
}

/// Owner authority only. At most sixteen blobs / 64 MiB verification per
/// transaction, one body and header prefix at a time. A returned cursor is
/// publishable only after known COMMIT; replay replaces identical derived data.
pub fn run(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    account: AccountId,
    after: Option<BlobId>,
    maximum: usize,
) -> Result<Report, Error> {
    run_with_options(
        request,
        trust,
        deadline,
        epoch,
        account,
        Options {
            after,
            maximum,
            structure: false,
        },
    )
}

#[derive(Clone, Copy)]
pub struct Options {
    pub after: Option<BlobId>,
    pub maximum: usize,
    pub structure: bool,
}

pub fn run_with_options(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    account: AccountId,
    options: Options,
) -> Result<Report, Error> {
    use crate::ports::BlobReader;
    let Options {
        after,
        maximum,
        structure,
    } = options;
    if request.mode != postgres_probe::Mode::Preparation {
        return Err(Error::Owner);
    }
    if !(1..=16).contains(&maximum) {
        return Err(Error::Invalid);
    }
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_body_worker(request, trust, deadline, |worker| {
        let rows = worker.run(move |client| Box::pin(async move {
            crate::postgres_maintenance::owner(client).await.map_err(|_| Error::Owner)?;
            let receipt = postgres_schema::lock_and_inspect(client).await.map_err(Error::Schema)?;
            if receipt.store_epoch != epoch { return Err(Error::Invalid); }
            client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ; SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_| Error::Database)?;
            if client.query_typed_opt("SELECT id FROM td_mta.accounts WHERE id=$1", &[(&account.as_bytes().as_slice(), Type::BYTEA)]).await.map_err(|_| Error::Database)?.is_none() { return Err(Error::Invalid); }
            let after = after.map(|id| *id.as_bytes());
            client.query_typed("SELECT a.sequence,a.floor,b.length,b.digest,b.created_at,b.chunk_count,b.changed,(SELECT count(*) FROM td_mta.blob_chunks c WHERE c.account=b.account AND c.blob_id=b.id),b.id FROM td_mta.accounts a JOIN td_mta.blobs b ON b.account=a.id WHERE a.id=$1 AND ($2::bytea IS NULL OR b.id>$2) AND EXISTS(SELECT 1 FROM td_mta.emails e WHERE e.account=b.account AND e.blob_id=b.id) ORDER BY b.id LIMIT $3", &[(&account.as_bytes().as_slice(), Type::BYTEA), (&after.as_ref().map(|id| id.as_slice()), Type::BYTEA), (&((maximum + 1) as i64), Type::INT8)]).await.map_err(|_| Error::Database)
        })).map_err(Error::Connection)??;
        let mut report = Report {
            scanned: 0,
            written: 0,
            skipped: 0,
            structures_written: 0,
            structures_skipped: 0,
            verified_bytes: 0,
            after,
            done: rows.len() <= maximum,
        };
        let mut output_bytes = 0usize;
        for row in rows.iter().take(maximum) {
            let id: &[u8] = row.try_get(8).map_err(|_| Error::Invalid)?;
            let blob = BlobId::from_bytes(id.try_into().map_err(|_| Error::Invalid)?);
            let metadata = postgres_body::blob_metadata(
                row,
                account,
                epoch,
                blob,
                postgres_body::MAX_BODY_BYTES,
            )
            .map_err(Error::Body)?;
            if report.scanned != 0
                && report.verified_bytes + metadata.body.length > postgres_body::MAX_BODY_BYTES
            {
                report.done = false;
                break;
            }
            let (fields, interpretation) =
                postgres_body::with_verified_pin(worker, metadata, deadline, |body| {
                    let length = usize::try_from(
                        body.len()
                            .min((postgres_summaries::HEADER_LIMIT + 2) as u64),
                    )
                    .map_err(|_| crate::ports::Error::Invalid)?;
                    let mut prefix = Vec::new();
                    prefix
                        .try_reserve_exact(length)
                        .map_err(|_| crate::ports::Error::Invalid)?;
                    prefix.resize(length, 0);
                    let mut read = 0;
                    while read < length {
                        let count = body.read_at(
                            read as u64,
                            prefix.get_mut(read..).ok_or(crate::ports::Error::Invalid)?,
                        )?;
                        if count == 0 {
                            return Err(crate::ports::Error::Corrupt);
                        }
                        read = read
                            .checked_add(count)
                            .filter(|n| *n <= length)
                            .ok_or(crate::ports::Error::Corrupt)?;
                    }
                    let fields = postgres_summaries::derive(
                        &prefix,
                        prefix.len() as u64 == metadata.body.length,
                        deadline,
                    );
                    let interpretation = if structure {
                        crate::postgres_mime_evidence::prepare_verified(
                            body,
                            &metadata.body.digest,
                            deadline,
                        )?
                    } else {
                        None
                    };
                    Ok((fields, interpretation))
                })
                .map_err(Error::Body)?;
            report.verified_bytes += metadata.body.length;
            let size = fields
                .as_ref()
                .map_or(0, |fields| fields.iter().map(Vec::len).sum::<usize>())
                + interpretation
                    .as_ref()
                    .map_or(0, |value| value.table.len() + value.chunks.len() + 32);
            if output_bytes + size > 256 * 1024 {
                report.done = false;
                break;
            }
            output_bytes += size;
            if let Some(fields) = fields {
                worker.run(move |client| Box::pin(async move {
                    client.execute_typed("INSERT INTO td_mta.blob_summaries(account,blob_id,version,body_length,body_digest,fields) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(account,blob_id) DO UPDATE SET version=EXCLUDED.version,body_length=EXCLUDED.body_length,body_digest=EXCLUDED.body_digest,fields=EXCLUDED.fields", &[(&account.as_bytes().as_slice(), Type::BYTEA), (&blob.as_bytes().as_slice(), Type::BYTEA), (&postgres_summaries::VERSION, Type::INT4), (&(metadata.body.length as i64), Type::INT8), (&metadata.body.digest.as_slice(), Type::BYTEA), (&fields, Type::BYTEA_ARRAY)]).await
                })).map_err(Error::Connection)?.map_err(|_| Error::Database)?;
                report.written += 1;
            } else {
                report.skipped += 1;
            }
            if let Some(value) = interpretation {
                worker.run(move |client| Box::pin(async move {
                    client.execute_typed("INSERT INTO td_mta.blob_interpretations(account,blob_id,version,body_length,body_digest,header_bytes,part_table,chunk_hashes,manifest_digest) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT(account,blob_id) DO UPDATE SET version=EXCLUDED.version,body_length=EXCLUDED.body_length,body_digest=EXCLUDED.body_digest,header_bytes=EXCLUDED.header_bytes,part_table=EXCLUDED.part_table,chunk_hashes=EXCLUDED.chunk_hashes,manifest_digest=EXCLUDED.manifest_digest", &[(&account.as_bytes().as_slice(), Type::BYTEA), (&blob.as_bytes().as_slice(), Type::BYTEA), (&crate::postgres_mime_evidence::VERSION, Type::INT4), (&(metadata.body.length as i64), Type::INT8), (&metadata.body.digest.as_slice(), Type::BYTEA), (&(value.header_bytes as i64), Type::INT8), (&value.table, Type::BYTEA), (&value.chunks, Type::BYTEA), (&value.digest.as_slice(), Type::BYTEA)]).await
                })).map_err(Error::Connection)?.map_err(|_| Error::Database)?;
                report.structures_written += 1;
            } else if structure {
                report.structures_skipped += 1;
            }
            report.scanned += 1;
            report.after = Some(blob);
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
