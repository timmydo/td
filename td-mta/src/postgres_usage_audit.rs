//! Historical quota-counter consistency; not full semantic verification.
use crate::{
    format::Sequence,
    ids::{AccountId, StoreEpoch},
    ports::ViewIdentity,
    postgres_probe::{self, Request},
    postgres_schema,
};
use std::time::Instant;
use td_crypto::TrustStore;
use tokio_postgres::Client;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Epoch,
    Missing,
    Integrity,
    Database,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Usage {
    pub body_bytes: u64,
    pub blob_count: u64,
    pub upload_bytes: u64,
    pub queue_bytes: u64,
    pub queue_submissions: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Report {
    pub identity: ViewIdentity,
    pub usage: Usage,
}

/// Compare all five durable counters in one finite read-only primary snapshot.
/// Expired but unreclaimed leases remain charged. This checks no body digests,
/// delivery outcomes, policy, credentials or service-admission authority.
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    account: AccountId,
) -> Result<Report, Error> {
    let result=postgres_probe::with_client(request,trust,deadline,move |client| Box::pin(async move {
        let schema=postgres_schema::lock_and_inspect(client).await.map_err(Error::Schema)?;
        if schema.store_epoch!=epoch { return Err(Error::Epoch); }
        client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_|Error::Database)?;
        let rows=client.query("SELECT sequence,floor FROM td_mta.accounts WHERE id=$1 LIMIT 2",&[&account.as_bytes().as_slice()]).await.map_err(|_|Error::Database)?;
        if rows.is_empty() { return Err(Error::Missing); }
        if rows.len()!=1 { return Err(Error::Integrity); }
        let row=rows.first().ok_or(Error::Integrity)?;
        let committed_sequence=sequence(row.try_get(0).map_err(|_|Error::Integrity)?)?;
        let history_floor=sequence(row.try_get(1).map_err(|_|Error::Integrity)?)?;
        if history_floor>committed_sequence { return Err(Error::Integrity); }
        let usage=audit(client,account).await?;
        client.batch_execute("ROLLBACK").await.map_err(|_|Error::Database)?;
        Ok(Report {identity:ViewIdentity {account,epoch,committed_sequence,history_floor},usage})
    })).map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}
fn sequence(bytes: &[u8]) -> Result<Sequence, Error> {
    Ok(Sequence::from_u64(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    )))
}

// Caller owns one stable snapshot. This can compose with further semantic checks.
pub(crate) async fn audit(client: &Client, account: AccountId) -> Result<Usage, Error> {
    let account = account.as_bytes().as_slice();
    let stored=client.query("SELECT body_bytes,blob_count,upload_bytes,queue_bytes,queue_submissions FROM td_mta.account_usage WHERE account=$1 LIMIT 2",&[&account]).await.map_err(|_|Error::Database)?;
    if stored.len() != 1 {
        return Err(Error::Integrity);
    }
    let stored = stored.first().ok_or(Error::Integrity)?;
    let observed=client.query_one("SELECT coalesce(sum(b.length),0)::bigint,count(*),coalesce(sum(b.length) FILTER (WHERE EXISTS (SELECT 1 FROM td_mta.leases l WHERE l.account=b.account AND l.blob_id=b.id)),0)::bigint,coalesce(sum(b.length) FILTER (WHERE EXISTS (SELECT 1 FROM td_mta.submissions s WHERE s.account=b.account AND s.transmitted_blob_id=b.id)),0)::bigint,(SELECT count(*) FROM td_mta.submissions s WHERE s.account=$1) FROM td_mta.blobs b WHERE b.account=$1",&[&account]).await.map_err(|_|Error::Database)?;
    let mut values = [0; 5];
    for (column, value) in values.iter_mut().enumerate() {
        let expected: i64 = stored.try_get(column).map_err(|_| Error::Integrity)?;
        let actual: i64 = observed.try_get(column).map_err(|_| Error::Integrity)?;
        if expected != actual {
            return Err(Error::Integrity);
        }
        *value = u64::try_from(actual).map_err(|_| Error::Integrity)?;
    }
    let [body_bytes, blob_count, upload_bytes, queue_bytes, queue_submissions] = values;
    if upload_bytes > body_bytes || queue_bytes > body_bytes {
        return Err(Error::Integrity);
    }
    Ok(Usage {
        body_bytes,
        blob_count,
        upload_bytes,
        queue_bytes,
        queue_submissions,
    })
}
