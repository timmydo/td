//! Route-scoped verified outgoing bytes; reading grants no SMTP ownership.
use crate::{
    format::{row::BlobRow, Sequence},
    ids::{BlobId, SubmissionId},
    ports::{self, BlobReader, ViewIdentity},
    postgres_body::{self, BlobMetadata},
    postgres_probe::{self, Request},
    postgres_runtime_profile::Binding,
    postgres_schema,
};
use std::time::Instant;
use td_crypto::TrustStore;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Body(postgres_body::Error),
    Epoch,
    Policy,
    Missing,
    Integrity,
    Capacity,
    Database,
}

pub struct ReadRequest<'a> {
    pub binding: &'a Binding,
    pub submission: SubmissionId,
    pub maximum: u64,
}

/// A finite primary snapshot admits only the selected relay profile's named
/// destination. The callback sees verified immutable bytes, never a claim;
/// transmission must separately acquire and retain current attempt authority.
pub fn with_submission<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    selected: ReadRequest<'_>,
    read: impl FnOnce(BlobMetadata, &mut dyn BlobReader) -> Result<T, ports::Error>,
) -> Result<T, Error> {
    let ReadRequest {
        binding,
        submission,
        maximum,
    } = selected;
    if !binding.roles().relay() {
        return Err(Error::Policy);
    }
    if maximum > postgres_body::MAX_BODY_BYTES {
        return Err(Error::Capacity);
    }
    let binding = binding.clone();
    postgres_probe::with_body_worker(request, trust, deadline, |worker| {
        let metadata = worker.run(|client| Box::pin(async move {
            let schema = postgres_schema::lock_and_inspect(client).await.map_err(Error::Schema)?;
            if schema.store_epoch != binding.epoch() { return Err(Error::Epoch); }
            client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000")
                .await.map_err(|_| Error::Database)?;
            let selected = client.query_one("SELECT generation,version_id FROM td_mta.config_selection WHERE singleton=true", &[])
                .await.map_err(|_| Error::Database)?;
            let version: Option<&[u8]> = selected.try_get(1).map_err(|_| Error::Integrity)?;
            if number(selected.try_get(0).map_err(|_| Error::Integrity)?)? != binding.selection().generation
                || version != Some(binding.selection().version.as_bytes().as_slice()) { return Err(Error::Policy); }
            let rows = client.query("SELECT a.sequence,a.floor,s.transmitted_blob_id,s.changed,b.length,b.digest,b.created_at,b.chunk_count,b.changed,(SELECT count(*) FROM td_mta.blob_chunks c WHERE c.account=b.account AND c.blob_id=b.id) FROM td_mta.accounts a JOIN td_mta.submissions s ON s.account=a.id JOIN td_mta.submission_routes r ON r.account=s.account AND r.submission_id=s.id LEFT JOIN td_mta.blobs b ON b.account=s.account AND b.id=s.transmitted_blob_id WHERE a.id=$1 AND s.id=$2 AND r.route_name=$3 AND r.destination=$4 LIMIT 2",
                &[&binding.account().as_bytes().as_slice(), &submission.as_bytes().as_slice(), &binding.route().name(), &binding.route().destination().as_slice()])
                .await.map_err(|_| Error::Database)?;
            if rows.is_empty() { return Err(Error::Missing); }
            if rows.len() != 1 { return Err(Error::Integrity); }
            let row = rows.first().ok_or(Error::Integrity)?;
            let sequence = number(row.try_get(0).map_err(|_| Error::Integrity)?)?;
            let floor = number(row.try_get(1).map_err(|_| Error::Integrity)?)?;
            if floor > sequence || number(row.try_get(3).map_err(|_| Error::Integrity)?)? > sequence
                || number(row.try_get(8).map_err(|_| Error::Integrity)?)? > sequence { return Err(Error::Integrity); }
            let blob: &[u8] = row.try_get(2).map_err(|_| Error::Integrity)?;
            let length: i64 = row.try_get(4).map_err(|_| Error::Integrity)?;
            let length = u64::try_from(length).map_err(|_| Error::Integrity)?;
            if length > postgres_body::MAX_BODY_BYTES { return Err(Error::Integrity); }
            if length > maximum { return Err(Error::Capacity); }
            let digest: &[u8] = row.try_get(5).map_err(|_| Error::Integrity)?;
            let chunks: i64 = row.try_get(7).map_err(|_| Error::Integrity)?;
            let actual: i64 = row.try_get(9).map_err(|_| Error::Integrity)?;
            if u64::try_from(chunks).ok() != Some(length.div_ceil(65536)) || chunks != actual { return Err(Error::Integrity); }
            Ok(BlobMetadata {
                identity: ViewIdentity { account: binding.account(), epoch: binding.epoch(), committed_sequence: Sequence::from_u64(sequence), history_floor: Sequence::from_u64(floor) },
                blob: BlobId::from_bytes(blob.try_into().map_err(|_| Error::Integrity)?),
                body: BlobRow { length, digest: digest.try_into().map_err(|_| Error::Integrity)?, created_at: row.try_get(6).map_err(|_| Error::Integrity)? },
            })
        })).map_err(Error::Connection)??;
        postgres_body::with_verified_body(worker, metadata, deadline, |body| read(metadata, body)).map_err(Error::Body)
    }).map_err(Error::Connection)?
}
fn number(bytes: &[u8]) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    ))
}
