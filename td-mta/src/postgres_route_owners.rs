//! Retained route-owner leases. No lease alone grants SMTP attempt authority.
use crate::{
    ids::{BootId, RouteClaimId},
    postgres_probe::{self, Request},
    postgres_runtime_profile::Binding,
    postgres_schema,
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

pub const LEASE_MS: i64 = 30_000;
const MAX_ROWS: i64 = 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Epoch,
    Policy,
    Capacity,
    Crypto,
    Integrity,
    Database,
    Conflict,
    Busy,
    Stale,
    Clock,
    Indeterminate,
}

#[derive(Clone)]
pub struct Claim {
    binding: Binding,
    boot: BootId,
    operation: RouteClaimId,
    digest: [u8; 32],
}
impl Claim {
    pub(crate) fn binding(&self) -> &Binding {
        &self.binding
    }
    pub fn operation(&self) -> RouteClaimId {
        self.operation
    }
    pub fn boot(&self) -> BootId {
        self.boot
    }
}
/// Historical receipt only; observe/renew must establish current ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Grant {
    pub generation: u64,
    pub claimed_at: i64,
    pub expires_at: i64,
}

pub fn prepare(binding: &Binding, boot: BootId) -> Result<Claim, Error> {
    if !binding.roles().relay() {
        return Err(Error::Policy);
    }
    let mut operation = [0; 16];
    SystemEntropy::try_new()
        .map_err(|_| Error::Crypto)?
        .fill(&mut operation)
        .map_err(|_| Error::Crypto)?;
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    for field in [
        b"td-mta route claim v1".as_slice(),
        binding.epoch().as_bytes().as_slice(),
        binding.account().as_bytes().as_slice(),
        binding.profile().as_bytes(),
        &binding.selection().generation.to_be_bytes(),
        binding.selection().version.as_bytes().as_slice(),
        binding.route().name().as_bytes(),
        binding.route().destination(),
        boot.as_bytes().as_slice(),
        &operation,
    ] {
        hash.update(&(field.len() as u64).to_be_bytes())
            .map_err(|_| Error::Crypto)?;
        hash.update(field).map_err(|_| Error::Crypto)?;
    }
    Ok(Claim {
        binding: binding.clone(),
        boot,
        operation: RouteClaimId::from_bytes(operation),
        digest: hash.finish().map_err(|_| Error::Crypto)?,
    })
}
fn number(bytes: &[u8]) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    ))
}
pub(crate) async fn begin(
    client: &Client,
    claim: &Claim,
    policy: bool,
    write: bool,
) -> Result<bool, Error> {
    if postgres_schema::lock_and_inspect(client)
        .await
        .map_err(Error::Schema)?
        .store_epoch
        != claim.binding.epoch()
    {
        return Err(Error::Epoch);
    }
    begin_transaction(client).await?;
    lock_account_policy(client, claim, policy, write).await
}
pub(crate) async fn begin_transaction(client: &Client) -> Result<(), Error> {
    client.batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_|Error::Database)
}
// Caller owns an open transaction and a matching schema/epoch qualification.
pub(crate) async fn lock_account_policy(
    client: &Client,
    claim: &Claim,
    policy: bool,
    write: bool,
) -> Result<bool, Error> {
    let current_policy = if policy {
        let row = client.query_one("SELECT generation,version_id FROM td_mta.config_selection WHERE singleton=true FOR SHARE", &[]).await.map_err(|_|Error::Database)?;
        let version: Option<&[u8]> = row.try_get(1).map_err(|_| Error::Integrity)?;
        claim.binding.roles().relay()
            && number(row.try_get(0).map_err(|_| Error::Integrity)?)?
                == claim.binding.selection().generation
            && version == Some(claim.binding.selection().version.as_bytes().as_slice())
    } else {
        true
    };
    let query = if write {
        "SELECT id FROM td_mta.accounts WHERE id=$1 FOR UPDATE"
    } else {
        "SELECT id FROM td_mta.accounts WHERE id=$1 FOR SHARE"
    };
    client
        .query_one(query, &[&claim.binding.account().as_bytes().as_slice()])
        .await
        .map_err(|_| Error::Database)?;
    Ok(current_policy)
}
async fn now(client: &Client) -> Result<i64, Error> {
    client
        .query_one(
            "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
            &[],
        )
        .await
        .map_err(|_| Error::Database)?
        .try_get(0)
        .map_err(|_| Error::Integrity)
}
async fn receipt(client: &Client, claim: &Claim) -> Result<Option<Grant>, Error> {
    let rows = client.query("SELECT request_digest,generation,claimed_at,expires_at FROM td_mta.route_owner_receipts WHERE account=$1 AND operation=$2", &[&claim.binding.account().as_bytes().as_slice(), &claim.operation.as_bytes().as_slice()]).await.map_err(|_|Error::Database)?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let digest: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
    if digest != claim.digest {
        return Err(Error::Conflict);
    }
    let generation = number(row.try_get(1).map_err(|_| Error::Integrity)?)?;
    let claimed_at: i64 = row.try_get(2).map_err(|_| Error::Integrity)?;
    let expires_at: i64 = row.try_get(3).map_err(|_| Error::Integrity)?;
    if generation == 0 || claimed_at.checked_add(LEASE_MS) != Some(expires_at) {
        return Err(Error::Integrity);
    }
    Ok(Some(Grant {
        generation,
        claimed_at,
        expires_at,
    }))
}
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    claim: &Claim,
) -> Result<Option<Grant>, Error> {
    let claim = claim.clone();
    postgres_probe::with_mailbox_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            begin(client, &claim, false, false).await?;
            receipt(client, &claim).await
        })
    })
    .map_err(Error::Connection)?
}
struct Owner {
    generation: u64,
    boot: Option<BootId>,
    operation: Option<RouteClaimId>,
    destination: [u8; 32],
    last_clock: i64,
    expires_at: Option<i64>,
}
async fn owner(client: &Client, claim: &Claim) -> Result<Option<Owner>, Error> {
    let rows = client.query("SELECT generation,owner_boot,operation,destination,last_clock,expires_at FROM td_mta.route_owners WHERE account=$1 AND route_name=$2 FOR UPDATE", &[&claim.binding.account().as_bytes().as_slice(), &claim.binding.route().name()]).await.map_err(|_|Error::Database)?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let boot: Option<&[u8]> = row.try_get(1).map_err(|_| Error::Integrity)?;
    let operation: Option<&[u8]> = row.try_get(2).map_err(|_| Error::Integrity)?;
    let destination: &[u8] = row.try_get(3).map_err(|_| Error::Integrity)?;
    let owner = Owner {
        generation: number(row.try_get(0).map_err(|_| Error::Integrity)?)?,
        boot: boot
            .map(|bytes| {
                bytes
                    .try_into()
                    .map(BootId::from_bytes)
                    .map_err(|_| Error::Integrity)
            })
            .transpose()?,
        operation: operation
            .map(|bytes| {
                bytes
                    .try_into()
                    .map(RouteClaimId::from_bytes)
                    .map_err(|_| Error::Integrity)
            })
            .transpose()?,
        destination: destination.try_into().map_err(|_| Error::Integrity)?,
        last_clock: row.try_get(4).map_err(|_| Error::Integrity)?,
        expires_at: row.try_get(5).map_err(|_| Error::Integrity)?,
    };
    if owner.generation == 0
        || owner.boot.is_some() != owner.operation.is_some()
        || owner.boot.is_some() != owner.expires_at.is_some()
        || owner
            .expires_at
            .is_some_and(|expiry| expiry <= owner.last_clock)
    {
        return Err(Error::Integrity);
    }
    Ok(Some(owner))
}
fn current(
    owner: &Owner,
    claim: &Claim,
    generation: u64,
    now: i64,
    unexpired: bool,
) -> Result<(), Error> {
    if now < owner.last_clock {
        return Err(Error::Clock);
    }
    if owner.generation != generation
        || owner.boot != Some(claim.boot)
        || owner.operation != Some(claim.operation)
        || owner.destination != *claim.binding.route().destination()
        || (unexpired && owner.expires_at.is_none_or(|expiry| expiry <= now))
    {
        return Err(Error::Stale);
    }
    Ok(())
}
/// The caller holds the configuration/account locks established by begin.
pub(crate) async fn check_locked(
    client: &Client,
    claim: &Claim,
    generation: u64,
) -> Result<i64, Error> {
    check_locked_observed(client, claim, generation, |_| Ok(())).await
}
/// Observe database UTC under the owner lock even when clock or lease checks
/// will refuse. A discontinuity must not disappear with the failed transaction.
pub(crate) async fn check_locked_observed<E: From<Error>>(
    client: &Client,
    claim: &Claim,
    generation: u64,
    observe: impl FnOnce(i64) -> Result<(), E>,
) -> Result<i64, E> {
    check_locked_expiry_observed(client, claim, generation, observe)
        .await
        .map(|(now, _)| now)
}
/// Same locked admission with the checked database lease expiry retained.
pub(crate) async fn check_locked_expiry_observed<E: From<Error>>(
    client: &Client,
    claim: &Claim,
    generation: u64,
    observe: impl FnOnce(i64) -> Result<(), E>,
) -> Result<(i64, i64), E> {
    let owner = owner(client, claim)
        .await
        .map_err(E::from)?
        .ok_or_else(|| E::from(Error::Stale))?;
    let timestamp = now(client).await.map_err(E::from)?;
    observe(timestamp)?;
    current(&owner, claim, generation, timestamp, true).map_err(E::from)?;
    Ok((
        timestamp,
        owner.expires_at.ok_or_else(|| E::from(Error::Integrity))?,
    ))
}

async fn commit(client: &Client, marker: &AtomicBool) -> Result<(), Error> {
    marker.store(true, Ordering::Relaxed);
    client
        .batch_execute("COMMIT")
        .await
        .map_err(|_| Error::Indeterminate)
}
fn finish<T>(
    result: Result<Result<T, Error>, postgres_probe::Error>,
    attempted: &AtomicBool,
) -> Result<T, Error> {
    match result {
        Ok(result) => result,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}
pub fn acquire(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    claim: &Claim,
) -> Result<Grant, Error> {
    if let Some(grant) = inspect(request, trust, deadline, claim)? {
        return Ok(grant);
    }
    let claim = claim.clone();
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_mutation_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let current_policy = begin(client, &claim, true, true).await?;
            if let Some(grant) = receipt(client, &claim).await? {
                return Ok(grant);
            }
            if !current_policy {
                return Err(Error::Policy);
            }
            let old = owner(client, &claim).await?;
            let timestamp = now(client).await?;
            let generation = if let Some(old) = &old {
                if timestamp < old.last_clock {
                    return Err(Error::Clock);
                }
                if old.expires_at.is_some_and(|expiry| expiry > timestamp) {
                    return Err(Error::Busy);
                }
                old.generation.checked_add(1).ok_or(Error::Capacity)?
            } else {
                1
            };
            let counts = client.query_one("SELECT (SELECT count(*) FROM (SELECT 1 FROM td_mta.route_owners WHERE account=$1 LIMIT 1024) owners),(SELECT count(*) FROM (SELECT 1 FROM td_mta.route_owner_receipts WHERE account=$1 LIMIT 1024) receipts)", &[&claim.binding.account().as_bytes().as_slice()]).await.map_err(|_|Error::Database)?;
            let owners: i64 = counts.try_get(0).map_err(|_| Error::Integrity)?;
            let receipts: i64 = counts.try_get(1).map_err(|_| Error::Integrity)?;
            if (old.is_none() && owners >= MAX_ROWS) || receipts >= MAX_ROWS {
                return Err(Error::Capacity);
            }
            let expires_at = timestamp.checked_add(LEASE_MS).ok_or(Error::Clock)?;
            let account = claim.binding.account();
            let account = account.as_bytes().as_slice();
            if client.execute("INSERT INTO td_mta.route_owners VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(account,route_name) DO UPDATE SET destination=EXCLUDED.destination,generation=EXCLUDED.generation,owner_boot=EXCLUDED.owner_boot,operation=EXCLUDED.operation,last_clock=EXCLUDED.last_clock,expires_at=EXCLUDED.expires_at",
            &[&account,&claim.binding.route().name(),&claim.binding.route().destination().as_slice(),&generation.to_be_bytes().as_slice(),&claim.boot.as_bytes().as_slice(),&claim.operation.as_bytes().as_slice(),&timestamp,&expires_at]).await.map_err(|_|Error::Database)? != 1 { return Err(Error::Integrity); }
            if client
                .execute(
                    "INSERT INTO td_mta.route_owner_receipts VALUES($1,$2,$3,$4,$5,$6)",
                    &[
                        &account,
                        &claim.operation.as_bytes().as_slice(),
                        &claim.digest.as_slice(),
                        &generation.to_be_bytes().as_slice(),
                        &timestamp,
                        &expires_at,
                    ],
                )
                .await
                .map_err(|_| Error::Database)?
                != 1
            {
                return Err(Error::Integrity);
            }
            commit(client, &marker).await?;
            Ok(Grant {
                generation,
                claimed_at: timestamp,
                expires_at,
            })
        })
    });
    finish(result, &attempted)
}
/// Observe under fresh policy; historical acquisition receipts are insufficient.
pub fn observe(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    claim: &Claim,
    generation: u64,
) -> Result<i64, Error> {
    let claim = claim.clone();
    postgres_probe::with_mailbox_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            if !begin(client, &claim, true, false).await? {
                return Err(Error::Policy);
            }
            let owner = owner(client, &claim).await?.ok_or(Error::Stale)?;
            current(&owner, &claim, generation, now(client).await?, true)?;
            owner.expires_at.ok_or(Error::Integrity)
        })
    })
    .map_err(Error::Connection)?
}
pub fn renew(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    claim: &Claim,
    generation: u64,
) -> Result<i64, Error> {
    update(request, trust, deadline, claim, generation, false)
}
/// The caller closes its owned SMTP transport before releasing coordination.
pub fn release(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    claim: &Claim,
    generation: u64,
) -> Result<(), Error> {
    update(request, trust, deadline, claim, generation, true).map(|_| ())
}
fn update(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    claim: &Claim,
    generation: u64,
    release: bool,
) -> Result<i64, Error> {
    let claim = claim.clone();
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_mutation_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            if !begin(client, &claim, !release, true).await? {
                return Err(Error::Policy);
            }
            let owner = owner(client, &claim).await?.ok_or(Error::Stale)?;
            let timestamp = now(client).await?;
            current(&owner, &claim, generation, timestamp, !release)?;
            let expires_at = timestamp.checked_add(LEASE_MS).ok_or(Error::Clock)?;
            let count = if release {
            client.execute("UPDATE td_mta.route_owners SET owner_boot=NULL,operation=NULL,expires_at=NULL,last_clock=$3 WHERE account=$1 AND route_name=$2", &[&claim.binding.account().as_bytes().as_slice(),&claim.binding.route().name(),&timestamp]).await
        } else {
            client.execute("UPDATE td_mta.route_owners SET expires_at=$3,last_clock=$4 WHERE account=$1 AND route_name=$2", &[&claim.binding.account().as_bytes().as_slice(),&claim.binding.route().name(),&expires_at,&timestamp]).await
        }.map_err(|_|Error::Database)?;
            if count != 1 {
                return Err(Error::Integrity);
            }
            commit(client, &marker).await?;
            Ok(expires_at)
        })
    });
    finish(result, &attempted)
}
