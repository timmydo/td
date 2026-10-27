//! Shared body and quota writes inside an already fenced account transaction.
use crate::{
    ids::{AccountId, BlobId},
    postgres_configuration::SharedQuotas,
};
use std::time::Instant;
use tokio_postgres::{types::Type, Client};

pub(crate) const CHUNK: usize = 65536;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Capacity,
    Quota,
    Integrity,
    Conflict,
    Database,
    Deadline,
}

pub(crate) struct Usage([i64; 5]);

/// The caller holds the account row FOR UPDATE until commit or rollback.
pub(crate) async fn charge(
    client: &Client,
    account: AccountId,
    caps: SharedQuotas,
    charges: [u64; 5],
) -> Result<Usage, Error> {
    let usage = client.query_typed_one("SELECT body_bytes,blob_count,upload_bytes,queue_bytes,queue_submissions FROM td_mta.account_usage WHERE account=$1", &[(&account.as_bytes().as_slice(), Type::BYTEA)]).await.map_err(|_| Error::Database)?;
    let limits = [
        caps.body_bytes,
        caps.blob_count,
        caps.upload_bytes,
        caps.queue_bytes,
        caps.queue_submissions,
    ];
    let mut totals = [0; 5];
    for (index, ((limit, charge), total)) in limits
        .into_iter()
        .zip(charges)
        .zip(totals.iter_mut())
        .enumerate()
    {
        let used: i64 = usage.try_get(index).map_err(|_| Error::Integrity)?;
        let used = u64::try_from(used).map_err(|_| Error::Integrity)?;
        let value = used
            .checked_add(charge)
            .filter(|value| *value <= limit)
            .ok_or(Error::Quota)?;
        *total = i64::try_from(value).map_err(|_| Error::Integrity)?;
    }
    Ok(Usage(totals))
}

impl Usage {
    /// Publish only in the same account transaction that computed the charge.
    pub(crate) async fn publish(self, client: &Client, account: AccountId) -> Result<(), Error> {
        let [body, blobs, uploads, queue, submissions] = self.0;
        if client.execute_typed("UPDATE td_mta.account_usage SET body_bytes=$2,blob_count=$3,upload_bytes=$4,queue_bytes=$5,queue_submissions=$6 WHERE account=$1", &[(&account.as_bytes().as_slice(), Type::BYTEA),(&body, Type::INT8),(&blobs, Type::INT8),(&uploads, Type::INT8),(&queue, Type::INT8),(&submissions, Type::INT8)]).await.map_err(|_|Error::Database)?!=1 { return Err(Error::Integrity); }
        Ok(())
    }
}

/// Prepared immutable bytes and their verified digest; no input is read from a
/// remote peer while this transaction holds its account lock.
pub(crate) struct Blob<'a> {
    pub account: AccountId,
    pub id: BlobId,
    pub bytes: &'a [u8],
    pub digest: &'a [u8; 32],
    pub created_at: i64,
    pub changed: u64,
}

pub(crate) async fn write_blob(
    client: &Client,
    value: Blob<'_>,
    deadline: Instant,
) -> Result<(), Error> {
    if value.bytes.len() as u64 > crate::postgres_body::MAX_BODY_BYTES {
        return Err(Error::Capacity);
    }
    let account = value.account.as_bytes().as_slice();
    let blob = value.id.as_bytes().as_slice();
    let length = i64::try_from(value.bytes.len()).map_err(|_| Error::Capacity)?;
    let chunks = i64::try_from(value.bytes.len().div_ceil(CHUNK)).map_err(|_| Error::Capacity)?;
    if client
        .execute_typed(
            "INSERT INTO td_mta.blob_ids VALUES($1,$2) ON CONFLICT DO NOTHING",
            &[(&account, Type::BYTEA), (&blob, Type::BYTEA)],
        )
        .await
        .map_err(|_| Error::Database)?
        != 1
    {
        return Err(Error::Conflict);
    }
    if client
        .execute_typed(
            "INSERT INTO td_mta.blobs VALUES($1,$2,$3,$4,$5,$6,$7)",
            &[
                (&account, Type::BYTEA),
                (&blob, Type::BYTEA),
                (&length, Type::INT8),
                (&value.digest.as_slice(), Type::BYTEA),
                (&value.created_at, Type::INT8),
                (&value.changed.to_be_bytes().as_slice(), Type::BYTEA),
                (&chunks, Type::INT8),
            ],
        )
        .await
        .map_err(|_| Error::Database)?
        != 1
    {
        return Err(Error::Integrity);
    }
    let mut batch = Vec::new();
    batch.try_reserve_exact(8).map_err(|_| Error::Capacity)?;
    for (index, bytes) in value.bytes.chunks(CHUNK * 8).enumerate() {
        if Instant::now() >= deadline {
            return Err(Error::Deadline);
        }
        batch.clear();
        batch.extend(bytes.chunks(CHUNK));
        let ordinal = i64::try_from(index * 8).map_err(|_| Error::Capacity)?;
        if client.execute_typed("INSERT INTO td_mta.blob_chunks SELECT $1,$2,$3+ordinality-1,bytes FROM unnest($4::bytea[]) WITH ORDINALITY chunks(bytes,ordinality)", &[(&account, Type::BYTEA),(&blob, Type::BYTEA),(&ordinal, Type::INT8),(&batch, Type::BYTEA_ARRAY)]).await.map_err(|_|Error::Database)?!=batch.len() as u64 { return Err(Error::Integrity); }
    }
    if let Some(fields) = crate::postgres_summaries::derive(value.bytes, true, deadline) {
        if client.execute_typed("INSERT INTO td_mta.blob_summaries(account,blob_id,version,body_length,body_digest,fields) VALUES($1,$2,$3,$4,$5,$6)", &[(&account, Type::BYTEA), (&blob, Type::BYTEA), (&crate::postgres_summaries::VERSION, Type::INT4), (&length, Type::INT8), (&value.digest.as_slice(), Type::BYTEA), (&fields, Type::BYTEA_ARRAY)]).await.map_err(|_| Error::Database)? != 1 {
            return Err(Error::Integrity);
        }
    }
    if Instant::now() >= deadline {
        return Err(Error::Deadline);
    }
    Ok(())
}

pub(crate) async fn select_thread(
    client: &Client,
    account: AccountId,
    requested: &[String],
    fresh: crate::ids::ThreadId,
    old: u64,
) -> Result<(crate::ids::ThreadId, bool), Error> {
    let mut anchors = Vec::new();
    anchors
        .try_reserve_exact(requested.len())
        .map_err(|_| Error::Capacity)?;
    anchors.extend(requested.iter().map(String::as_str));
    let rows=client.query_typed("SELECT a.email_id,e.thread_id,a.changed,e.changed,t.changed FROM unnest($2::text[]) WITH ORDINALITY requested(message_id,position) JOIN LATERAL (SELECT email_id,changed FROM td_mta.thread_anchors WHERE account=$1 AND message_id=requested.message_id ORDER BY email_id LIMIT 1) a ON true LEFT JOIN td_mta.emails e ON e.account=$1 AND e.id=a.email_id LEFT JOIN td_mta.threads t ON t.account=e.account AND t.id=e.thread_id ORDER BY requested.position LIMIT 1", &[(&account.as_bytes().as_slice(), Type::BYTEA),(&anchors, Type::TEXT_ARRAY)]).await.map_err(|_|Error::Database)?;
    let Some(row) = rows.first() else {
        return Ok((fresh, true));
    };
    let email: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
    let thread: &[u8] = row.try_get(1).map_err(|_| Error::Integrity)?;
    if email.len() != 16
        || (2..=4).any(|column| {
            row.try_get::<_, &[u8]>(column)
                .ok()
                .and_then(|bytes| bytes.try_into().ok().map(u64::from_be_bytes))
                .is_none_or(|changed| changed > old)
        })
    {
        return Err(Error::Integrity);
    }
    Ok((
        crate::ids::ThreadId::from_bytes(thread.try_into().map_err(|_| Error::Integrity)?),
        false,
    ))
}
