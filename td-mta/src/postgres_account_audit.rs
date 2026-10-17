//! Audit canonical metadata, mailbox-role uniqueness, usage and bodies.
use crate::{
    format::Table,
    ids::{AccountId, BlobId, StoreEpoch},
    metadata_sweep::{self, CompleteMetadata},
    postgres_body, postgres_metadata_audit,
    postgres_probe::{self, Request, Worker},
    postgres_usage_audit::{self, Usage},
};
use std::time::Instant;
use td_crypto::TrustStore;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub metadata: metadata_sweep::Limits,
    pub blobs: u64,
    pub bytes: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Metadata(postgres_metadata_audit::Error),
    Usage(postgres_usage_audit::Error),
    Body(postgres_body::Error),
    Integrity,
    Capacity,
    Database,
}
/// Historical only; excludes role registry, configuration, credentials,
/// history and retained receipts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Report {
    metadata: CompleteMetadata,
    usage: Usage,
    blobs: u64,
    bytes: u64,
}
impl Report {
    pub const fn metadata(self) -> CompleteMetadata {
        self.metadata
    }
    pub const fn usage(self) -> Usage {
        self.usage
    }
    pub const fn blobs(self) -> u64 {
        self.blobs
    }
    pub const fn bytes(self) -> u64 {
        self.bytes
    }
}

/// Check canonical metadata, role uniqueness, counters and every body in one
/// read-only primary snapshot. This is not restore or service admission.
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    account: AccountId,
    utc_ms: i64,
    limits: Limits,
) -> Result<Report, Error> {
    let result = postgres_probe::with_body_worker(request, trust, deadline, |worker| {
        let metadata =
            postgres_metadata_audit::audit(worker, epoch, account, utc_ms, limits.metadata)
                .map_err(Error::Metadata)?;
        let duplicate_roles = worker
            .run(move |client| {
                Box::pin(async move {
                    client.query_one("SELECT EXISTS (SELECT 1 FROM td_mta.mailboxes WHERE account=$1 AND role IS NOT NULL GROUP BY role COLLATE \"C\" HAVING count(*)>1)", &[&account.as_bytes().as_slice()]).await
                })
            })
            .map_err(Error::Connection)?
            .map_err(|_| Error::Database)?
            .try_get::<_, bool>(0)
            .map_err(|_| Error::Integrity)?;
        if duplicate_roles {
            return Err(Error::Integrity);
        }
        let usage = worker
            .run(move |client| {
                Box::pin(async move { postgres_usage_audit::audit(client, account).await })
            })
            .map_err(Error::Connection)?
            .map_err(Error::Usage)?;
        let (blobs, bytes) = bodies(worker, metadata, deadline, limits)?;
        if metadata.references().table_rows(Table::Blobs) != Some(blobs)
            || usage.blob_count != blobs
            || usage.body_bytes != bytes
        {
            return Err(Error::Integrity);
        }
        worker
            .run(|client| Box::pin(async move { client.batch_execute("ROLLBACK").await }))
            .map_err(Error::Connection)?
            .map_err(|_| Error::Database)?;
        Ok(Report {
            metadata,
            usage,
            blobs,
            bytes,
        })
    })
    .map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}

fn bodies(
    worker: &mut Worker,
    metadata: CompleteMetadata,
    deadline: Instant,
    limits: Limits,
) -> Result<(u64, u64), Error> {
    let identity = metadata.references().identity();
    let mut after: Option<BlobId> = None;
    let mut blobs = 0u64;
    let mut bytes = 0u64;
    loop {
        let body=worker.run(move |client|Box::pin(async move {
            let after_bytes=after.map(|id|id.as_bytes().to_vec());
            let rows=client.query("SELECT a.sequence,a.floor,b.length,b.digest,b.created_at,b.chunk_count,b.changed,(SELECT count(*) FROM td_mta.blob_chunks c WHERE c.account=b.account AND c.blob_id=b.id),b.id FROM td_mta.accounts a JOIN td_mta.blobs b ON b.account=a.id WHERE a.id=$1 AND ($2::bytea IS NULL OR b.id>$2) ORDER BY b.id LIMIT 1",&[&identity.account.as_bytes().as_slice(),&after_bytes]).await.map_err(|_|Error::Database)?;
            let Some(row)=rows.first() else {return Ok(None)};
            let id:&[u8]=row.try_get(8).map_err(|_|Error::Integrity)?;
            let id=BlobId::from_bytes(id.try_into().map_err(|_|Error::Integrity)?);
            if after.is_some_and(|prior|prior>=id) {return Err(Error::Integrity);}
            let body=postgres_body::blob_metadata(row,identity.account,identity.epoch,id,postgres_body::MAX_BODY_BYTES).map_err(Error::Body)?;
            if body.identity!=identity {return Err(Error::Integrity);}
            Ok(Some(body))
        })).map_err(Error::Connection)??;
        let Some(body) = body else {
            return Ok((blobs, bytes));
        };
        let next_blobs = blobs.checked_add(1).ok_or(Error::Capacity)?;
        let next_bytes = bytes.checked_add(body.body.length).ok_or(Error::Capacity)?;
        if next_blobs > limits.blobs || next_bytes > limits.bytes {
            return Err(Error::Capacity);
        }
        postgres_body::with_verified_body(worker, body, deadline, |_| Ok(()))
            .map_err(Error::Body)?;
        blobs = next_blobs;
        bytes = next_bytes;
        after = Some(body.blob);
    }
}
