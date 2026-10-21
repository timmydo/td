//! Explicit repair of a shared route hold after a newer selected configuration.
use crate::{
    ids::RouteRepairId,
    postgres_probe::{self, Request},
    postgres_route_health::{self as health, Error, Outcome},
    postgres_route_owners::{self as owners, Claim},
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
use td_crypto::{Crypto, Digest, Entropy, Provider, SystemEntropy, TrustStore};
use tokio_postgres::Client;
#[derive(Clone)]
pub struct Repair {
    claim: Claim,
    generation: u64,
    operation: RouteRepairId,
    digest: [u8; 32],
}
impl Repair {
    pub fn operation(&self) -> RouteRepairId {
        self.operation
    }
}
/// Prepare only for an explicitly requested route-repair operation. Merely
/// loading configuration must not publish this change or clear any hold.
pub fn prepare(claim: &Claim, generation: u64) -> Result<Repair, Error> {
    if generation == 0 {
        return Err(Error::Integrity);
    }
    let mut operation = [0; 16];
    SystemEntropy::try_new()
        .map_err(|_| Error::Crypto)?
        .fill(&mut operation)
        .map_err(|_| Error::Crypto)?;
    let binding = claim.binding();
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    for field in [
        b"td-mta route repair v1".as_slice(),
        binding.epoch().as_bytes().as_slice(),
        binding.account().as_bytes().as_slice(),
        binding.route().name().as_bytes(),
        binding.route().destination().as_slice(),
        claim.operation().as_bytes().as_slice(),
        claim.boot().as_bytes().as_slice(),
        &generation.to_be_bytes(),
        &binding.selection().generation.to_be_bytes(),
        &operation,
    ] {
        hash.update(&(field.len() as u64).to_be_bytes())
            .map_err(|_| Error::Crypto)?;
        hash.update(field).map_err(|_| Error::Crypto)?;
    }
    Ok(Repair {
        claim: claim.clone(),
        generation,
        operation: RouteRepairId::from_bytes(operation),
        digest: hash.finish().map_err(|_| Error::Crypto)?,
    })
}
async fn receipt(client: &Client, value: &Repair) -> Result<Option<Outcome>, Error> {
    let binding = value.claim.binding();
    let rows=client.query("SELECT request_digest,route_name,destination,action,old_revision,new_revision,failure_count,reason,failed_at,next_at,repair_generation FROM td_mta.route_health_receipts WHERE account=$1 AND operation=$2", &[&binding.account().as_bytes().as_slice(),&value.operation.as_bytes().as_slice()]).await.map_err(|_|Error::Database)?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let digest: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
    if digest != value.digest {
        return Err(Error::Conflict);
    }
    let route: &str = row.try_get(1).map_err(|_| Error::Integrity)?;
    let destination: &[u8] = row.try_get(2).map_err(|_| Error::Integrity)?;
    let action: i64 = row.try_get(3).map_err(|_| Error::Integrity)?;
    let old = health::number(row.try_get(4).map_err(|_| Error::Integrity)?)?;
    let new = health::number(row.try_get(5).map_err(|_| Error::Integrity)?)?;
    let state = health::state(row, 6, new)?;
    if route != binding.route().name()
        || destination != binding.route().destination()
        || action != 2
        || old == 0
        || old.checked_add(1) != Some(new)
        || state.failure_count == 0
        || !matches!(state.reason, 4 | 5)
        || state.repair_generation.is_some()
    {
        return Err(Error::Integrity);
    }
    Ok(Some(Outcome {
        old_revision: old,
        state,
    }))
}
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Repair,
) -> Result<Option<Outcome>, Error> {
    let value = value.clone();
    postgres_probe::with_mailbox_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            owners::begin(client, &value.claim, false, false)
                .await
                .map_err(Error::Owner)?;
            receipt(client, &value).await
        })
    })
    .map_err(Error::Connection)?
}
/// Fresh observation for an explicit process-wide repair request. An account
/// without a hold needs no mutation; this observation grants no delivery rights.
pub fn needed(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    claim: &Claim,
    generation: u64,
) -> Result<bool, Error> {
    let claim = claim.clone();
    postgres_probe::with_mailbox_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            if !owners::begin(client, &claim, true, false)
                .await
                .map_err(Error::Owner)?
            {
                return Err(Error::Policy);
            }
            owners::check_locked(client, &claim, generation)
                .await
                .map_err(Error::Owner)?;
            Ok(health::current(client, &claim)
                .await?
                .is_some_and(|state| state.repair_generation.is_some()))
        })
    })
    .map_err(Error::Connection)?
}
pub fn publish(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Repair,
) -> Result<Outcome, Error> {
    if let Some(outcome) = inspect(request, trust, deadline, value)? {
        return Ok(outcome);
    }
    let value = value.clone();
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_mutation_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let policy = owners::begin(client, &value.claim, true, true)
                .await
                .map_err(Error::Owner)?;
            if let Some(outcome) = receipt(client, &value).await? {
                return Ok(outcome);
            }
            if !policy {
                return Err(Error::Policy);
            }
            let timestamp = owners::check_locked(client, &value.claim, value.generation)
                .await
                .map_err(Error::Owner)?;
            let mut state = health::current(client, &value.claim)
                .await?
                .ok_or(Error::Missing)?;
            let held = state.repair_generation.ok_or(Error::Conflict)?;
            let binding = value.claim.binding();
            if binding.selection().generation <= held {
                return Err(Error::RepairRequired);
            }
            if state.failed_at.is_none_or(|failed| timestamp < failed) {
                return Err(Error::Clock);
            }
            let account = binding.account();
            let account = account.as_bytes().as_slice();
            let count:i64=client.query_one("SELECT count(*) FROM (SELECT 1 FROM td_mta.route_health_receipts WHERE account=$1 LIMIT 1024) r", &[&account]).await.map_err(|_|Error::Database)?.try_get(0).map_err(|_|Error::Integrity)?;
            if count >= 1024 {
                return Err(Error::Capacity);
            }
            let old = state.revision;
            state.revision = old.checked_add(1).ok_or(Error::Capacity)?;
            state.repair_generation = None;
            let revision = state.revision.to_be_bytes();
            let revision = revision.as_slice();
            if client.execute("UPDATE td_mta.route_health SET revision=$4,repair_generation=NULL WHERE account=$1 AND route_name=$2 AND destination=$3", &[&account,&binding.route().name(),&binding.route().destination().as_slice(),&revision]).await.map_err(|_|Error::Database)?!=1 {return Err(Error::Integrity);}
            client.execute("INSERT INTO td_mta.route_health_receipts VALUES($1,$2,$3,$4,$5,2,$6,$7,$8,$9,$10,$11,NULL)", &[&account,&value.operation.as_bytes().as_slice(),&value.digest.as_slice(),&binding.route().name(),&binding.route().destination().as_slice(),&old.to_be_bytes().as_slice(),&revision,&i64::from(state.failure_count),&state.reason,&state.failed_at,&state.next_at]).await.map_err(|_|Error::Database)?;
            let final_time = owners::check_locked(client, &value.claim, value.generation)
                .await
                .map_err(Error::Owner)?;
            if final_time < timestamp {
                return Err(Error::Clock);
            }
            marker.store(true, Ordering::Relaxed);
            client
                .batch_execute("COMMIT")
                .await
                .map_err(|_| Error::Indeterminate)?;
            Ok(Outcome {
                old_revision: old,
                state,
            })
        })
    });
    match result {
        Ok(result) => result,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}
