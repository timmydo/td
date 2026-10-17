//! Historical verification of one immutable body, not whole-store integrity.
use crate::{
    ids::{AccountId, BlobId, StoreEpoch},
    postgres_body::{self, BlobMetadata},
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
    Body(postgres_body::Error),
    Database,
}

/// Verify one account's manifest and body in one finite read-only primary
/// snapshot. Database access is the caller's administrative authority; this
/// is not a device-authorized download and never returns body bytes.
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    account: AccountId,
    blob: BlobId,
) -> Result<BlobMetadata, Error> {
    let result = postgres_probe::with_body_worker(request, trust, deadline, |worker| {
        let metadata = worker.run(|client| Box::pin(async move {
            let schema=postgres_schema::lock_and_inspect(client).await.map_err(Error::Schema)?;
            if schema.store_epoch!=epoch { return Err(Error::Epoch); }
            client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_|Error::Database)?;
            let rows=client.query("SELECT a.sequence,a.floor,b.length,b.digest,b.created_at,b.chunk_count,b.changed,(SELECT count(*) FROM td_mta.blob_chunks c WHERE c.account=b.account AND c.blob_id=b.id) FROM td_mta.accounts a JOIN td_mta.blobs b ON b.account=a.id WHERE a.id=$1 AND b.id=$2 LIMIT 2",&[&account.as_bytes().as_slice(),&blob.as_bytes().as_slice()]).await.map_err(|_|Error::Database)?;
            if rows.is_empty() { return Err(Error::Missing); }
            if rows.len()!=1 { return Err(Error::Body(postgres_body::Error::Integrity)); }
            postgres_body::blob_metadata(rows.first().ok_or(Error::Missing)?,account,epoch,blob,postgres_body::MAX_BODY_BYTES).map_err(Error::Body)
        })).map_err(Error::Connection)??;
        postgres_body::with_verified_body(worker,metadata,deadline,|_|Ok(())).map_err(Error::Body)?;
        worker.run(|client|Box::pin(async move {client.batch_execute("ROLLBACK").await})).map_err(Error::Connection)?.map_err(|_|Error::Database)?;
        Ok(metadata)
    }).map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}
