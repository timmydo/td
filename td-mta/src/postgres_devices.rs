//! Owner-side device preparation. Observations grant no serving authority.
use crate::{
    device_credentials::{
        self, Credential, DevicePassword, Presented, DEVICE_CAPACITY, DEVICE_RECORDS,
    },
    ids::{AccountId, DeviceId, DeviceOperationId, StoreEpoch},
    postgres_probe::{self, Request},
    postgres_schema,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime},
};
use td_crypto::{Crypto, Digest, Entropy, Provider, SystemEntropy, TrustStore};
use tokio_postgres::{types::Type, Client};

#[path = "postgres_device_list.rs"]
mod listing;
pub use listing::{list, DeviceSummary};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Schema(postgres_schema::Error),
    Connection(postgres_probe::Error),
    Epoch,
    Missing,
    Invalid,
    Conflict,
    Capacity,
    /// The Creation outlived `CREATION_LIFETIME` without a stored receipt.
    Stale,
    Crypto,
    Database,
    /// Retain the same prepared operation. The original COMMIT may settle later.
    Indeterminate,
}

/// A revoked device row is deleted once its revocation, stamped by the database
/// clock, is older than this; no record then holds its credential.
pub const REVOKED_RETENTION_MS: i64 = 30 * 24 * 3_600_000;
/// A Creation without a stored receipt is refused once this old. A retired row
/// was revoked after it was inserted, so its Creation is over 30 days old and
/// can never be published again.
pub const CREATION_LIFETIME: Duration = Duration::from_secs(24 * 3600);

/// When a Creation was prepared, on two clocks: the monotonic one misses
/// suspend and snapshot-restore, which the wall clock counts.
#[derive(Clone, Copy)]
struct Clocks {
    monotonic: Instant,
    wall: SystemTime,
}
impl Clocks {
    fn now() -> Self {
        Self {
            monotonic: Instant::now(),
            wall: SystemTime::now(),
        }
    }
}

/// Past `CREATION_LIFETIME` on either clock. A wall clock set backwards
/// leaves the monotonic bound.
fn stale(prepared: Clocks, now: Clocks) -> bool {
    now.monotonic.saturating_duration_since(prepared.monotonic) > CREATION_LIFETIME
        || now
            .wall
            .duration_since(prepared.wall)
            .is_ok_and(|age| age > CREATION_LIFETIME)
}

/// Retain before publication, including after an uncertain response. The
/// password is intentionally neither Debug nor Clone and is never sent to SQL.
pub struct Creation {
    epoch: StoreEpoch,
    account: AccountId,
    credential: Credential,
    password: DevicePassword,
    label: String,
    created: i64,
    digest: [u8; 32],
    prepared: Clocks,
}
impl Creation {
    pub fn device_id(&self) -> DeviceId {
        self.credential.id
    }
    pub fn password(&self) -> &[u8] {
        self.password.expose()
    }
}

/// Prepare once; retaining the same value preserves the creation identity and
/// request digest across uncertainty. Preparation does not contact a database.
pub fn prepare_creation(
    epoch: StoreEpoch,
    account: AccountId,
    label: &str,
    created_ms: u64,
) -> Result<Creation, Error> {
    device_credentials::valid_label(label).map_err(|_| Error::Invalid)?;
    let created = i64::try_from(created_ms).map_err(|_| Error::Invalid)?;
    let mut entropy = SystemEntropy::try_new().map_err(|_| Error::Crypto)?;
    let (credential, password) =
        device_credentials::generate(account, &mut entropy).map_err(|_| Error::Crypto)?;
    let digest = hash(&[
        b"td-mta PostgreSQL device creation v1\0",
        epoch.as_bytes(),
        account.as_bytes(),
        credential.id.as_bytes(),
        &credential.salt,
        &credential.verifier,
        &created.to_be_bytes(),
        label.as_bytes(),
    ])?;
    Ok(Creation {
        epoch,
        account,
        credential,
        password,
        label: label.into(),
        created,
        digest,
        prepared: Clocks::now(),
    })
}

/// Contains only public operation metadata and its request digest.
#[derive(Clone, Copy)]
pub struct Revocation {
    epoch: StoreEpoch,
    account: AccountId,
    device: DeviceId,
    id: DeviceOperationId,
    digest: [u8; 32],
}
impl Revocation {
    pub fn operation_id(&self) -> DeviceOperationId {
        self.id
    }
}
pub fn prepare_revocation(
    epoch: StoreEpoch,
    account: AccountId,
    device: DeviceId,
    revoked_ms: u64,
) -> Result<Revocation, Error> {
    let revoked = i64::try_from(revoked_ms).map_err(|_| Error::Invalid)?;
    let mut id = [0; 16];
    SystemEntropy::try_new()
        .and_then(|mut entropy| entropy.fill(&mut id))
        .map_err(|_| Error::Crypto)?;
    let id = DeviceOperationId::from_bytes(id);
    let digest = hash(&[
        b"td-mta PostgreSQL device revocation v1\0",
        epoch.as_bytes(),
        account.as_bytes(),
        device.as_bytes(),
        id.as_bytes(),
        &revoked.to_be_bytes(),
    ])?;
    Ok(Revocation {
        epoch,
        account,
        device,
        id,
        digest,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevocationOutcome {
    /// This exact retained operation has a durable receipt.
    Recorded,
    /// Another operation already revoked this device. This call wrote nothing.
    AlreadyRevoked,
}

struct CreationRow {
    id: DeviceId,
    salt: [u8; 32],
    verifier: [u8; 32],
    label: String,
    created: i64,
    digest: [u8; 32],
    stale: bool,
}
enum Action {
    Create(CreationRow),
    Revoke(Revocation),
}
enum Change {
    Known(RevocationOutcome),
    Write(RevocationOutcome),
}

pub fn publish_creation(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    creation: &Creation,
) -> Result<(), Error> {
    let row = CreationRow {
        id: creation.credential.id,
        salt: creation.credential.salt,
        verifier: creation.credential.verifier,
        label: creation.label.clone(),
        created: creation.created,
        digest: creation.digest,
        stale: stale(creation.prepared, Clocks::now()),
    };
    mutate(
        request,
        trust,
        deadline,
        creation.epoch,
        creation.account,
        Action::Create(row),
    )
    .map(|_| ())
}

pub fn publish_revocation(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    revocation: &Revocation,
) -> Result<RevocationOutcome, Error> {
    mutate(
        request,
        trust,
        deadline,
        revocation.epoch,
        revocation.account,
        Action::Revoke(*revocation),
    )
}

fn mutate(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    account: AccountId,
    action: Action,
) -> Result<RevocationOutcome, Error> {
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    // Retirement and the record counts outgrow the ordinary 128-frame budget.
    let result = postgres_probe::with_mutation_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            begin(client, epoch).await?;
            // Every device writer takes the account fence before device rows.
            let rows = client
                .query(
                    "SELECT id FROM td_mta.accounts WHERE id=$1 FOR UPDATE",
                    &[&account.as_bytes().as_slice()],
                )
                .await
                .map_err(|_| Error::Database)?;
            if rows.len() != 1 {
                return Err(Error::Missing);
            }
            let change = match action {
                Action::Create(row) => create(client, account, row).await?,
                Action::Revoke(revocation) => revoke(client, revocation).await?,
            };
            match change {
                Change::Known(outcome) => Ok(outcome),
                Change::Write(outcome) => {
                    marker.store(true, Ordering::Relaxed);
                    client
                        .batch_execute("COMMIT")
                        .await
                        .map_err(|_| Error::Indeterminate)?;
                    Ok(outcome)
                }
            }
        })
    });
    match result {
        Ok(result) => result,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}

async fn begin(client: &Client, epoch: StoreEpoch) -> Result<(), Error> {
    let receipt = postgres_schema::lock_and_inspect(client)
        .await
        .map_err(Error::Schema)?;
    if receipt.store_epoch != epoch {
        return Err(Error::Epoch);
    }
    client
        .batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=10000")
        .await
        .map_err(|_| Error::Database)
}

async fn create(client: &Client, account: AccountId, row: CreationRow) -> Result<Change, Error> {
    let rows = client
        .query(
            "SELECT creation_digest FROM td_mta.devices WHERE account=$1 AND id=$2 LIMIT 2",
            &[
                &account.as_bytes().as_slice(),
                &row.id.as_bytes().as_slice(),
            ],
        )
        .await
        .map_err(|_| Error::Database)?;
    if let Some(existing) = rows.first() {
        let stored: &[u8] = existing.try_get(0).map_err(|_| Error::Invalid)?;
        if rows.len() != 1 || stored != row.digest {
            return Err(Error::Conflict);
        }
        return Ok(Change::Known(RevocationOutcome::Recorded));
    }
    if row.stale {
        return Err(Error::Stale);
    }
    // Rows are bounded by DEVICE_RECORDS, so this deletes at most that many.
    client
        .execute(
            "DELETE FROM td_mta.devices WHERE account=$1 AND revoked_at<(extract(epoch FROM clock_timestamp())*1000)::bigint-$2::bigint",
            &[&account.as_bytes().as_slice(), &REVOKED_RETENTION_MS],
        )
        .await
        .map_err(|_| Error::Database)?;
    let limit = i64::try_from(DEVICE_RECORDS.checked_add(1).ok_or(Error::Capacity)?)
        .map_err(|_| Error::Capacity)?;
    // Active rows are a subset of the bounded scan, exact whenever it is not full.
    let counts = client
        .query_one(
            "SELECT count(*) FILTER (WHERE revoked_at IS NULL),count(*) FROM (SELECT revoked_at FROM td_mta.devices WHERE account=$1 LIMIT $2) bounded",
            &[&account.as_bytes().as_slice(), &limit],
        )
        .await
        .map_err(|_| Error::Database)?;
    for (column, cap) in [(0, DEVICE_CAPACITY), (1, DEVICE_RECORDS)] {
        let count: i64 = counts.try_get(column).map_err(|_| Error::Invalid)?;
        if usize::try_from(count).map_err(|_| Error::Invalid)? >= cap {
            return Err(Error::Capacity);
        }
    }
    let written = client.execute("INSERT INTO td_mta.devices (account,id,salt,verifier,label,created_at,creation_digest) VALUES ($1,$2,$3,$4,$5,$6,$7)",
        &[&account.as_bytes().as_slice(), &row.id.as_bytes().as_slice(), &row.salt.as_slice(),
        &row.verifier.as_slice(), &row.label, &row.created, &row.digest.as_slice()])
        .await.map_err(|_| Error::Database)?;
    if written != 1 {
        return Err(Error::Invalid);
    }
    Ok(Change::Write(RevocationOutcome::Recorded))
}

async fn revoke(client: &Client, operation: Revocation) -> Result<Change, Error> {
    let rows = client.query("SELECT id,revocation_digest FROM td_mta.devices WHERE account=$1 AND revocation_id=$2 LIMIT 2",
        &[&operation.account.as_bytes().as_slice(), &operation.id.as_bytes().as_slice()])
        .await.map_err(|_| Error::Database)?;
    if let Some(row) = rows.first() {
        let id: &[u8] = row.try_get(0).map_err(|_| Error::Invalid)?;
        let digest: &[u8] = row.try_get(1).map_err(|_| Error::Invalid)?;
        if rows.len() != 1 || id != operation.device.as_bytes() || digest != operation.digest {
            return Err(Error::Conflict);
        }
        return Ok(Change::Known(RevocationOutcome::Recorded));
    }
    let rows = client
        .query(
            "SELECT revoked_at FROM td_mta.devices WHERE account=$1 AND id=$2 LIMIT 2",
            &[
                &operation.account.as_bytes().as_slice(),
                &operation.device.as_bytes().as_slice(),
            ],
        )
        .await
        .map_err(|_| Error::Database)?;
    let row = rows.first().ok_or(Error::Missing)?;
    if rows.len() != 1 {
        return Err(Error::Invalid);
    }
    let revoked: Option<i64> = row.try_get(0).map_err(|_| Error::Invalid)?;
    if revoked.is_some() {
        return Ok(Change::Known(RevocationOutcome::AlreadyRevoked));
    }
    // The database clock, not the requester's, dates the revocation: retention
    // counts from it.
    let written = client.execute("UPDATE td_mta.devices SET revoked_at=(extract(epoch FROM clock_timestamp())*1000)::bigint,revocation_id=$3,revocation_digest=$4 WHERE account=$1 AND id=$2 AND revoked_at IS NULL",
        &[&operation.account.as_bytes().as_slice(), &operation.device.as_bytes().as_slice(),
        &operation.id.as_bytes().as_slice(), &operation.digest.as_slice()])
        .await.map_err(|_| Error::Database)?;
    if written != 1 {
        return Err(Error::Invalid);
    }
    Ok(Change::Write(RevocationOutcome::Recorded))
}

/// Cold credential observation on a fresh primary snapshot. This closes the
/// snapshot before return and grants no authority for a later mail operation.
pub fn check(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    account: AccountId,
    password: &[u8],
) -> Result<Option<DeviceId>, Error> {
    let Some(presented) = Presented::parse(password) else {
        return Ok(None);
    };
    let result = postgres_probe::with_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let receipt = postgres_schema::lock_and_inspect(client).await.map_err(Error::Schema)?;
            if receipt.store_epoch != epoch { return Err(Error::Epoch); }
            client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_| Error::Database)?;
            authenticate(client, account, presented).await
        })
    }).map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}

// Caller owns an admitted primary transaction; never reuse the result outside it.
pub(crate) async fn authenticate(
    client: &Client,
    account: AccountId,
    presented: Presented,
) -> Result<Option<DeviceId>, Error> {
    let rows = client.query_typed("SELECT salt,verifier,revoked_at FROM td_mta.devices WHERE account=$1 AND id=$2 LIMIT 2",
                &[(&account.as_bytes().as_slice(), Type::BYTEA), (&presented.id().as_bytes().as_slice(), Type::BYTEA)])
                .await.map_err(|_| Error::Database)?;
    if rows.len() > 1 {
        return Err(Error::Invalid);
    }
    let (salt, expected, active) = if let Some(row) = rows.first() {
        let salt: &[u8] = row.try_get(0).map_err(|_| Error::Invalid)?;
        let expected: &[u8] = row.try_get(1).map_err(|_| Error::Invalid)?;
        let revoked: Option<i64> = row.try_get(2).map_err(|_| Error::Invalid)?;
        (
            salt.try_into().map_err(|_| Error::Invalid)?,
            expected.try_into().map_err(|_| Error::Invalid)?,
            revoked.is_none(),
        )
    } else {
        ([0; 32], [0; 32], false)
    };
    let matched = presented
        .matches(account, &salt, &expected)
        .map_err(|_| Error::Crypto)?;
    Ok((active && matched).then_some(presented.id()))
}

fn hash(parts: &[&[u8]]) -> Result<[u8; 32], Error> {
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    for part in parts {
        hash.update(part).map_err(|_| Error::Crypto)?;
    }
    hash.finish().map_err(|_| Error::Crypto)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn creations_go_stale_after_their_lifetime_on_either_clock() {
        let prepared = Clocks::now();
        let after = |monotonic: Duration, wall: Duration| Clocks {
            monotonic: prepared.monotonic + monotonic,
            wall: prepared.wall + wall,
        };
        let over = CREATION_LIFETIME + Duration::from_millis(1);
        let zero = Duration::ZERO;
        assert!(!stale(prepared, prepared));
        assert!(!stale(
            prepared,
            after(CREATION_LIFETIME, CREATION_LIFETIME)
        ));
        assert!(stale(prepared, after(over, zero)));
        // Suspend or snapshot-restore: only the wall clock moved on.
        assert!(stale(prepared, after(zero, over)));
        // A wall clock set backwards, or an earlier sample, stays fresh.
        let earlier = Clocks {
            monotonic: prepared.monotonic,
            wall: prepared.wall - Duration::from_secs(3600),
        };
        assert!(!stale(prepared, earlier));
        assert!(!stale(after(Duration::from_secs(1), zero), prepared));
    }
}
