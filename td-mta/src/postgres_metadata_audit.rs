//! Historical canonical metadata checks, not whole-store or restore authority.
use crate::{
    format::{Sequence, MAX_KEY_BYTES, MAX_VALUE_BYTES},
    ids::{AccountId, StoreEpoch},
    metadata_sweep::{self, CompleteMetadata, Limits},
    ports::ViewIdentity,
    postgres_probe::{self, Request},
    postgres_schema,
};
use std::{sync::Mutex, time::Instant};
use td_crypto::TrustStore;

#[path = "postgres_metadata_view.rs"]
mod view;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Epoch,
    Missing,
    Integrity,
    Database,
    Metadata(metadata_sweep::Error),
}

/// Check all canonical metadata tables in one finite read-only primary snapshot.
/// UTC is trusted caller input for lease-reference policy. This neither audits
/// body bytes nor grants device, serving, delivery or restore authority.
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    account: AccountId,
    utc_ms: i64,
    limits: Limits,
) -> Result<CompleteMetadata, Error> {
    let budget = postgres_probe::AuditBudget::new(limits, 0, 0).map_err(Error::Connection)?;
    let result = postgres_probe::with_audit_worker(request, trust, deadline, budget, |worker| {
        let report = audit(worker, epoch, account, utc_ms, limits)?;
        worker
            .run(|client| Box::pin(async move { client.batch_execute("ROLLBACK").await }))
            .map_err(Error::Connection)?
            .map_err(|_| Error::Database)?;
        Ok(report)
    })
    .map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}

// Begins and retains the snapshot for composition by other closed audits.
pub(crate) fn audit(
    worker: &mut postgres_probe::Worker,
    epoch: StoreEpoch,
    account: AccountId,
    utc_ms: i64,
    limits: Limits,
) -> Result<CompleteMetadata, Error> {
    let identity=worker.run(move |client| Box::pin(async move {
        let schema=postgres_schema::lock_and_inspect(client).await.map_err(Error::Schema)?;
        if schema.store_epoch!=epoch {return Err(Error::Epoch);}
        client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_|Error::Database)?;
        let rows=client.query("SELECT sequence,floor FROM td_mta.accounts WHERE id=$1 LIMIT 2",&[&account.as_bytes().as_slice()]).await.map_err(|_|Error::Database)?;
        if rows.is_empty() {return Err(Error::Missing);}
        if rows.len()!=1 {return Err(Error::Integrity);}
        let row=rows.first().ok_or(Error::Integrity)?;
        let sequence=|index|->Result<Sequence,Error>{let bytes:&[u8]=row.try_get(index).map_err(|_|Error::Integrity)?;Ok(Sequence::from_u64(u64::from_be_bytes(bytes.try_into().map_err(|_|Error::Integrity)?)))};
        let committed_sequence=sequence(0)?;
        let history_floor=sequence(1)?;
        if history_floor>committed_sequence {return Err(Error::Integrity);}
        Ok(ViewIdentity{account,epoch,committed_sequence,history_floor})
    })).map_err(Error::Connection)??;
    let mut sweep = metadata_sweep::Sweep::new(identity, utc_ms, limits);
    let mut key = [0; MAX_KEY_BYTES];
    let mut value = vec![0; MAX_VALUE_BYTES];
    {
        let mut view = view::View {
            identity,
            worker: Mutex::new(&mut *worker),
            failure: None,
        };
        while sweep
            .advance(&mut view, &mut key, &mut value)
            .map_err(|error| {
                view.failure
                    .map_or(Error::Metadata(error), Error::Connection)
            })?
            != metadata_sweep::Step::Complete
        {}
    }
    let report = sweep.finish().map_err(Error::Metadata)?;
    Ok(report)
}
