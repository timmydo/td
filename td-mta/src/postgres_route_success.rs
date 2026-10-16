//! Shared route recovery after durably accepted final DATA.
use crate::{
    ids::AttemptId,
    postgres_probe::{self, Request},
    postgres_route_health::{self as health, Error, Outcome, State},
    postgres_route_owners::{self as owners, Claim},
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
use td_crypto::{Crypto, Digest, Provider, TrustStore};
use tokio_postgres::Client;
#[derive(Clone)]
pub struct Change {
    claim: Claim,
    generation: u64,
    attempt: AttemptId,
    digest: [u8; 32],
}
/// Bind one successful-route reset to the accepted attempt identity.
pub fn prepare(claim: &Claim, generation: u64, attempt: AttemptId) -> Result<Change, Error> {
    if generation == 0 {
        return Err(Error::Integrity);
    }
    let binding = claim.binding();
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    for field in [
        b"td-mta route success v1".as_slice(),
        binding.epoch().as_bytes().as_slice(),
        binding.account().as_bytes().as_slice(),
        binding.route().name().as_bytes(),
        binding.route().destination().as_slice(),
        claim.operation().as_bytes().as_slice(),
        claim.boot().as_bytes().as_slice(),
        &generation.to_be_bytes(),
        attempt.as_bytes().as_slice(),
    ] {
        hash.update(&(field.len() as u64).to_be_bytes())
            .map_err(|_| Error::Crypto)?;
        hash.update(field).map_err(|_| Error::Crypto)?;
    }
    Ok(Change {
        claim: claim.clone(),
        generation,
        attempt,
        digest: hash.finish().map_err(|_| Error::Crypto)?,
    })
}
async fn receipt(client: &Client, value: &Change) -> Result<Option<Outcome>, Error> {
    let binding = value.claim.binding();
    let rows=client.query("SELECT request_digest,route_name,destination,action,old_revision,new_revision,failure_count,reason,failed_at,next_at,repair_generation FROM td_mta.route_health_receipts WHERE account=$1 AND operation=$2", &[&binding.account().as_bytes().as_slice(),&value.attempt.as_bytes().as_slice()]).await.map_err(|_|Error::Database)?;
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
        || action != 3
        || old.checked_add(1) != Some(new)
        || state.failure_count != 0
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
    value: &Change,
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
/// The exact attempt must already have a retained acceptance receipt and be
/// retired. This never changes recipient results or clears a repair hold.
pub fn publish(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Change,
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
            if timestamp < 0 {
                return Err(Error::Clock);
            }
            let binding = value.claim.binding();
            let account = binding.account();
            let account = account.as_bytes().as_slice();
            let rows=client.query("SELECT generation,owner_boot,owner_operation,prepared_at,active,EXISTS(SELECT 1 FROM td_mta.attempt_acceptance_receipts r WHERE r.account=submission_attempts.account AND r.attempt_id=submission_attempts.attempt_id AND r.phase=4) FROM td_mta.submission_attempts WHERE account=$1 AND attempt_id=$2 AND route_name=$3 AND destination=$4", &[&account,&value.attempt.as_bytes().as_slice(),&binding.route().name(),&binding.route().destination().as_slice()]).await.map_err(|_|Error::Database)?;
            let row = rows.first().ok_or(Error::Missing)?;
            let generation = health::number(row.try_get(0).map_err(|_| Error::Integrity)?)?;
            let boot: &[u8] = row.try_get(1).map_err(|_| Error::Integrity)?;
            let operation: &[u8] = row.try_get(2).map_err(|_| Error::Integrity)?;
            let prepared: i64 = row.try_get(3).map_err(|_| Error::Integrity)?;
            let active: bool = row.try_get(4).map_err(|_| Error::Integrity)?;
            let accepted: bool = row.try_get(5).map_err(|_| Error::Integrity)?;
            if active || !accepted {
                return Err(Error::Conflict);
            }
            if generation != value.generation
                || boot != value.claim.boot().as_bytes()
                || operation != value.claim.operation().as_bytes()
            {
                return Err(Error::Conflict);
            }
            if prepared < 0 || prepared > timestamp {
                return Err(Error::Clock);
            }
            let prior = health::current(client, &value.claim).await?;
            if let Some(prior) = prior {
                if prior.repair_generation.is_some() {
                    return Err(Error::RepairRequired);
                }
                if prior
                    .failed_at
                    .is_some_and(|last| prepared < last || timestamp < last)
                {
                    return Err(Error::Conflict);
                }
            }
            let limits=client.query_one("SELECT (SELECT count(*) FROM (SELECT 1 FROM td_mta.route_health WHERE account=$1 LIMIT 1024) h),(SELECT count(*) FROM (SELECT 1 FROM td_mta.route_health_receipts WHERE account=$1 LIMIT 1024) r)", &[&account]).await.map_err(|_|Error::Database)?;
            let health_count: i64 = limits.try_get(0).map_err(|_| Error::Integrity)?;
            let receipts: i64 = limits.try_get(1).map_err(|_| Error::Integrity)?;
            if (prior.is_none() && health_count >= 1024) || receipts >= 1024 {
                return Err(Error::Capacity);
            }
            let old = prior.map_or(0, |state| state.revision);
            let revision = old.checked_add(1).ok_or(Error::Capacity)?;
            let state = State {
                revision,
                failure_count: 0,
                reason: 0,
                failed_at: None,
                next_at: None,
                repair_generation: None,
            };
            let revision_bytes = revision.to_be_bytes();
            let revision_sql = revision_bytes.as_slice();
            if client.execute("INSERT INTO td_mta.route_health VALUES($1,$2,$3,$4,0,0,NULL,NULL,NULL) ON CONFLICT(account,route_name,destination) DO UPDATE SET revision=EXCLUDED.revision,failure_count=0,reason=0,failed_at=NULL,next_at=NULL,repair_generation=NULL", &[&account,&binding.route().name(),&binding.route().destination().as_slice(),&revision_sql]).await.map_err(|_|Error::Database)?!=1 {return Err(Error::Integrity);}
            client.execute("INSERT INTO td_mta.route_health_receipts VALUES($1,$2,$3,$4,$5,3,$6,$7,0,0,NULL,NULL,NULL)", &[&account,&value.attempt.as_bytes().as_slice(),&value.digest.as_slice(),&binding.route().name(),&binding.route().destination().as_slice(),&old.to_be_bytes().as_slice(),&revision_sql]).await.map_err(|_|Error::Database)?;
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
