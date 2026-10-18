//! Shared route failure publication; transport and repair activation remain separate.
use crate::{
    ids::AttemptId,
    postgres_probe::{self, Request},
    postgres_route_owners::{self as owners, Claim},
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
use td_crypto::{Crypto, Digest, Provider, SystemEntropy, TrustStore};
use tokio_postgres::{Client, Row};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Owner(owners::Error),
    Policy,
    Missing,
    Conflict,
    Integrity,
    Capacity,
    Crypto,
    Clock,
    RepairRequired,
    Backoff,
    Database,
    Indeterminate,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Failure {
    Network,
    Tls,
    Auth,
    Protocol,
}
impl Failure {
    fn code(self) -> i64 {
        match self {
            Self::Network => 3,
            Self::Tls => 4,
            Self::Auth => 5,
            Self::Protocol => 8,
        }
    }
}
#[derive(Clone)]
pub struct Change {
    claim: Claim,
    generation: u64,
    attempt: AttemptId,
    failure: Failure,
    digest: [u8; 32],
}
impl Change {
    pub(crate) fn attempt(&self) -> AttemptId {
        self.attempt
    }
    pub(crate) fn same_request(&self, other: &Self) -> bool {
        self.attempt == other.attempt && self.digest == other.digest
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct State {
    pub revision: u64,
    pub failure_count: u32,
    pub reason: i64,
    pub failed_at: Option<i64>,
    pub next_at: Option<i64>,
    pub repair_generation: Option<u64>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Outcome {
    pub old_revision: u64,
    pub state: State,
}
/// The attempt identity is the operation key: preparing again cannot count the
/// same route failure twice or replace its classification after publication.
pub fn prepare(
    claim: &Claim,
    generation: u64,
    attempt: AttemptId,
    failure: Failure,
) -> Result<Change, Error> {
    if generation == 0 {
        return Err(Error::Integrity);
    }
    let binding = claim.binding();
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    for field in [
        b"td-mta route failure v1".as_slice(),
        binding.epoch().as_bytes().as_slice(),
        binding.account().as_bytes().as_slice(),
        binding.route().name().as_bytes(),
        binding.route().destination().as_slice(),
        claim.operation().as_bytes().as_slice(),
        claim.boot().as_bytes().as_slice(),
        &generation.to_be_bytes(),
        attempt.as_bytes().as_slice(),
        &failure.code().to_be_bytes(),
    ] {
        hash.update(&(field.len() as u64).to_be_bytes())
            .map_err(|_| Error::Crypto)?;
        hash.update(field).map_err(|_| Error::Crypto)?;
    }
    Ok(Change {
        claim: claim.clone(),
        generation,
        attempt,
        failure,
        digest: hash.finish().map_err(|_| Error::Crypto)?,
    })
}
pub(crate) fn number(bytes: &[u8]) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    ))
}
pub(crate) fn state(row: &Row, offset: usize, revision: u64) -> Result<State, Error> {
    let count: i64 = row.try_get(offset).map_err(|_| Error::Integrity)?;
    let reason: i64 = row.try_get(offset + 1).map_err(|_| Error::Integrity)?;
    let failed: Option<i64> = row.try_get(offset + 2).map_err(|_| Error::Integrity)?;
    let next: Option<i64> = row.try_get(offset + 3).map_err(|_| Error::Integrity)?;
    let repair: Option<&[u8]> = row.try_get(offset + 4).map_err(|_| Error::Integrity)?;
    let repair = repair.map(number).transpose()?;
    let count = u32::try_from(count).map_err(|_| Error::Integrity)?;
    if revision == 0
        || !matches!(reason, 0 | 3 | 4 | 5 | 8)
        || repair.is_some_and(|g| g == 0 || !matches!(reason, 4 | 5))
    {
        return Err(Error::Integrity);
    }
    if count == 0 {
        if reason != 0 || failed.is_some() || next.is_some() || repair.is_some() {
            return Err(Error::Integrity);
        }
    } else {
        let failed = failed.ok_or(Error::Integrity)?;
        let next = next.ok_or(Error::Integrity)?;
        if failed < 0
            || next
                .checked_sub(failed)
                .is_none_or(|delay| !(300_000..=3_600_000).contains(&delay))
        {
            return Err(Error::Integrity);
        }
    }
    Ok(State {
        revision,
        failure_count: count,
        reason,
        failed_at: failed,
        next_at: next,
        repair_generation: repair,
    })
}
pub(crate) async fn current(client: &Client, claim: &Claim) -> Result<Option<State>, Error> {
    let binding = claim.binding();
    let rows=client.query_typed("SELECT revision,failure_count,reason,failed_at,next_at,repair_generation FROM td_mta.route_health WHERE account=$1 AND route_name=$2 AND destination=$3", &[(&binding.account().as_bytes().as_slice(),tokio_postgres::types::Type::BYTEA),(&binding.route().name(),tokio_postgres::types::Type::TEXT),(&binding.route().destination().as_slice(),tokio_postgres::types::Type::BYTEA)]).await.map_err(|_|Error::Database)?;
    rows.first()
        .map(|row| {
            state(
                row,
                1,
                number(row.try_get(0).map_err(|_| Error::Integrity)?)?,
            )
        })
        .transpose()
}
/// Caller holds current policy/account/route-owner locks. This supplements,
/// rather than replaces, queue-clock, recipient due-time and transport checks.
pub(crate) async fn admit_locked(
    client: &Client,
    claim: &Claim,
    timestamp: i64,
) -> Result<(), Error> {
    if let Some(state) = current(client, claim).await? {
        if state.failed_at.is_some_and(|failed| timestamp < failed) {
            return Err(Error::Clock);
        }
        if state.repair_generation.is_some() {
            return Err(Error::RepairRequired);
        }
        if state.next_at.is_some_and(|next| timestamp < next) {
            return Err(Error::Backoff);
        }
    }
    Ok(())
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
    let old = number(row.try_get(4).map_err(|_| Error::Integrity)?)?;
    let new = number(row.try_get(5).map_err(|_| Error::Integrity)?)?;
    let state = state(row, 6, new)?;
    if route != binding.route().name()
        || destination != binding.route().destination()
        || action != 1
        || old.checked_add(1) != Some(new)
        || state.failure_count == 0
        || state.reason != value.failure.code()
        || state.repair_generation.is_some()
            != matches!(value.failure, Failure::Tls | Failure::Auth)
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
/// Call only after closing/fencing the failed attempt's transport. This stores
/// route health before the attempt is retired; settle recipient responsibility
/// afterward through the attempt API. Historical receipts still replay.
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
    let mut entropy = SystemEntropy::try_new().map_err(|_| Error::Crypto)?;
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
            let rows=client.query("SELECT generation,owner_boot,owner_operation,prepared_at,active FROM td_mta.submission_attempts WHERE account=$1 AND attempt_id=$2 AND route_name=$3 AND destination=$4", &[&account,&value.attempt.as_bytes().as_slice(),&binding.route().name(),&binding.route().destination().as_slice()]).await.map_err(|_|Error::Database)?;
            let row = rows.first().ok_or(Error::Missing)?;
            let generation = number(row.try_get(0).map_err(|_| Error::Integrity)?)?;
            let boot: &[u8] = row.try_get(1).map_err(|_| Error::Integrity)?;
            let operation: &[u8] = row.try_get(2).map_err(|_| Error::Integrity)?;
            let prepared: i64 = row.try_get(3).map_err(|_| Error::Integrity)?;
            let active: bool = row.try_get(4).map_err(|_| Error::Integrity)?;
            if !active
                || generation != value.generation
                || boot != value.claim.boot().as_bytes()
                || operation != value.claim.operation().as_bytes()
            {
                return Err(Error::Conflict);
            }
            if prepared < 0 || prepared > timestamp {
                return Err(Error::Clock);
            }
            let prior = current(client, &value.claim).await?;
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
            let count = prior
                .map_or(0, |state| state.failure_count)
                .checked_add(1)
                .ok_or(Error::Capacity)?;
            let delay = crate::postgres_attempt_recovery::retry_delay(&mut entropy, count)
                .map_err(|error| match error {
                    crate::postgres_attempt_recovery::Error::Crypto => Error::Crypto,
                    _ => Error::Capacity,
                })?;
            let next = timestamp.checked_add(delay).ok_or(Error::Clock)?;
            let repair = if matches!(value.failure, Failure::Tls | Failure::Auth) {
                Some(binding.selection().generation)
            } else {
                None
            };
            if repair == Some(0) {
                return Err(Error::Integrity);
            }
            let repair_bytes = repair.map(u64::to_be_bytes);
            let repair_sql = repair_bytes.as_ref().map(|bytes| bytes.as_slice());
            let revision_bytes = revision.to_be_bytes();
            let revision_sql = revision_bytes.as_slice();
            let count_sql = i64::from(count);
            let reason = value.failure.code();
            if client.execute("INSERT INTO td_mta.route_health VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT(account,route_name,destination) DO UPDATE SET revision=EXCLUDED.revision,failure_count=EXCLUDED.failure_count,reason=EXCLUDED.reason,failed_at=EXCLUDED.failed_at,next_at=EXCLUDED.next_at,repair_generation=EXCLUDED.repair_generation", &[&account,&binding.route().name(),&binding.route().destination().as_slice(),&revision_sql,&count_sql,&reason,&timestamp,&next,&repair_sql]).await.map_err(|_|Error::Database)?!=1 {return Err(Error::Integrity);}
            client.execute("INSERT INTO td_mta.route_health_receipts VALUES($1,$2,$3,$4,$5,1,$6,$7,$8,$9,$10,$11,$12)", &[&account,&value.attempt.as_bytes().as_slice(),&value.digest.as_slice(),&binding.route().name(),&binding.route().destination().as_slice(),&old.to_be_bytes().as_slice(),&revision_sql,&count_sql,&reason,&timestamp,&next,&repair_sql]).await.map_err(|_|Error::Database)?;
            owners::check_locked(client, &value.claim, value.generation)
                .await
                .map_err(Error::Owner)?;
            marker.store(true, Ordering::Relaxed);
            client
                .batch_execute("COMMIT")
                .await
                .map_err(|_| Error::Indeterminate)?;
            Ok(Outcome {
                old_revision: old,
                state: State {
                    revision,
                    failure_count: count,
                    reason,
                    failed_at: Some(timestamp),
                    next_at: Some(next),
                    repair_generation: repair,
                },
            })
        })
    });
    match result {
        Ok(result) => result,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}
