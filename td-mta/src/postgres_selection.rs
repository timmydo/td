//! Owner-side selected configuration and retained compare-and-swap outcomes.
use crate::{
    ids::{ConfigurationSelectionId, StoreEpoch},
    postgres_configuration::Candidate,
    postgres_configuration_store::{self as versions, Version},
    postgres_probe::{self, Request},
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

pub const MAX_RECEIPTS: i64 = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Configuration(versions::Error),
    Epoch,
    Conflict,
    Integrity,
    Capacity,
    Crypto,
    Database,
    Indeterminate,
}

/// Passive selection; not policy or device authorization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    pub generation: u64,
    pub version: Version,
}

/// Retain this exact change across uncertain publication. The request digest
/// binds the epoch, expected generation, version and validated source digest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Change {
    epoch: StoreEpoch,
    operation: ConfigurationSelectionId,
    expected: u64,
    version: Version,
    source_digest: [u8; 32],
    request_digest: [u8; 32],
}
impl Change {
    pub fn operation(self) -> ConfigurationSelectionId {
        self.operation
    }
    pub fn version(self) -> Version {
        self.version
    }
}

pub fn prepare(
    epoch: StoreEpoch,
    expected: u64,
    version: Version,
    candidate: &Candidate,
) -> Result<Change, Error> {
    if candidate.epoch() != epoch {
        return Err(Error::Epoch);
    }
    expected.checked_add(1).ok_or(Error::Capacity)?;
    let mut id = [0; 16];
    SystemEntropy::try_new()
        .and_then(|mut rng| rng.fill(&mut id))
        .map_err(|_| Error::Crypto)?;
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    for part in [
        b"td-mta PostgreSQL selection v1\0".as_slice(),
        epoch.as_bytes().as_slice(),
        &expected.to_be_bytes(),
        version.as_bytes().as_slice(),
        candidate.digest().as_slice(),
    ] {
        hash.update(part).map_err(|_| Error::Crypto)?;
    }
    Ok(Change {
        epoch,
        operation: ConfigurationSelectionId::from_bytes(id),
        expected,
        version,
        source_digest: *candidate.digest(),
        request_digest: hash.finish().map_err(|_| Error::Crypto)?,
    })
}

fn selection(row: &tokio_postgres::Row) -> Result<Option<Selection>, Error> {
    let generation: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
    let generation = u64::from_be_bytes(generation.try_into().map_err(|_| Error::Integrity)?);
    let version: Option<&[u8]> = row.try_get(1).map_err(|_| Error::Integrity)?;
    match (generation, version) {
        (0, None) => Ok(None),
        (0, Some(_)) | (_, None) => Err(Error::Integrity),
        (generation, Some(version)) => Ok(Some(Selection {
            generation,
            version: Version::from_bytes(version.try_into().map_err(|_| Error::Integrity)?),
        })),
    }
}

async fn receipt(
    client: &tokio_postgres::Client,
    change: Change,
) -> Result<Option<Selection>, Error> {
    let rows = client.query("SELECT generation,version_id,request_digest FROM td_mta.config_selection_receipts WHERE operation=$1 LIMIT 2", &[&change.operation.as_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
    if rows.is_empty() {
        return Ok(None);
    }
    if rows.len() != 1 {
        return Err(Error::Integrity);
    }
    let row = rows.first().ok_or(Error::Integrity)?;
    let digest: &[u8] = row.try_get(2).map_err(|_| Error::Integrity)?;
    if digest != change.request_digest {
        return Err(Error::Conflict);
    }
    let selected = selection(row)?.ok_or(Error::Integrity)?;
    if Some(selected.generation) != change.expected.checked_add(1)
        || selected.version != change.version
    {
        return Err(Error::Integrity);
    }
    Ok(Some(selected))
}

/// Resolve a retained operation, even after a newer selection or later material
/// expiry. A shared policy lock excludes an earlier in-flight publication's
/// COMMIT before reading its receipt. A lock/connection failure leaves prior
/// uncertainty unresolved. Absence never authorizes a different operation ID.
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    change: Change,
) -> Result<Option<Selection>, Error> {
    postgres_probe::with_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let schema = postgres_schema::lock_and_inspect(client)
                .await
                .map_err(Error::Schema)?;
            if schema.store_epoch != change.epoch {
                return Err(Error::Epoch);
            }
            client.batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_| Error::Database)?;
            client.query_one("SELECT singleton FROM td_mta.config_selection WHERE singleton=true FOR SHARE", &[]).await.map_err(|_| Error::Database)?;
            receipt(client, change).await
        })
    })
    .map_err(Error::Connection)?
}

/// Revalidate the immutable source version, then atomically select it if the
/// generation still matches. Receipts survive newer selections. All writers
/// take this singleton policy row before any future account/device locks.
/// No service listener, runtime generation or role is activated here.
pub fn publish(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    change: Change,
) -> Result<Selection, Error> {
    if let Some(selected) = inspect(request, trust, deadline, change)? {
        return Ok(selected);
    }
    let candidate = versions::read(request, trust, deadline, change.epoch, change.version)
        .map_err(Error::Configuration)?;
    if candidate.digest() != &change.source_digest {
        return Err(Error::Conflict);
    }
    drop(candidate);
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let schema = postgres_schema::lock_and_inspect(client)
                .await
                .map_err(Error::Schema)?;
            if schema.store_epoch != change.epoch {
                return Err(Error::Epoch);
            }
            client.batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_| Error::Database)?;
            let row = client.query_one("SELECT generation,version_id FROM td_mta.config_selection WHERE singleton=true FOR UPDATE", &[]).await.map_err(|_| Error::Database)?;
            // A waiter must inspect the receipt after acquiring the publication
            // lock, so an absence observation cannot race a duplicate publication.
            if let Some(selected) = receipt(client, change).await? {
                return Ok(selected);
            }
            let current = selection(&row)?.map_or(0, |selected| selected.generation);
            if current != change.expected {
                return Err(Error::Conflict);
            }
            let count: i64 = client.query_one("SELECT count(*) FROM (SELECT operation FROM td_mta.config_selection_receipts LIMIT 1024) bounded", &[]).await.map_err(|_| Error::Database)?.try_get(0).map_err(|_| Error::Integrity)?;
            if count >= MAX_RECEIPTS {
                return Err(Error::Capacity);
            }
            let version_row = client
                .query_one(
                    "SELECT checksum FROM td_mta.config_versions WHERE id=$1",
                    &[&change.version.as_bytes().as_slice()],
                )
                .await
                .map_err(|_| Error::Database)?;
            let digest: &[u8] = version_row.try_get(0).map_err(|_| Error::Integrity)?;
            if digest != change.source_digest {
                return Err(Error::Conflict);
            }
            let generation = change.expected.checked_add(1).ok_or(Error::Capacity)?;
            let bytes = generation.to_be_bytes();
            let changed = client.execute("UPDATE td_mta.config_selection SET generation=$1,version_id=$2 WHERE singleton=true", &[&bytes.as_slice(), &change.version.as_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
            if changed != 1 {
                return Err(Error::Integrity);
            }
            let inserted = client.execute("INSERT INTO td_mta.config_selection_receipts (operation,request_digest,generation,version_id) VALUES ($1,$2,$3,$4)", &[&change.operation.as_bytes().as_slice(), &change.request_digest.as_slice(), &bytes.as_slice(), &change.version.as_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
            if inserted != 1 {
                return Err(Error::Integrity);
            }
            marker.store(true, Ordering::Relaxed);
            client
                .batch_execute("COMMIT")
                .await
                .map_err(|_| Error::Indeterminate)?;
            Ok(Selection {
                generation,
                version: change.version,
            })
        })
    });
    match result {
        Ok(result) => result,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}

/// Capture one selected version, then revalidate its immutable sources and
/// materials under the same deadline. A concurrent selection may supersede
/// the returned passive generation; runtime publication must fence it afresh.
pub fn read(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
) -> Result<Option<(Selection, Candidate)>, Error> {
    let selected = observe(request, trust, deadline, epoch)?;
    let Some(selected) = selected else {
        return Ok(None);
    };
    let candidate = versions::read(request, trust, deadline, epoch, selected.version)
        .map_err(Error::Configuration)?;
    Ok(Some((selected, candidate)))
}

/// One passive primary observation; never a runtime policy guard.
pub(crate) fn observe(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
) -> Result<Option<Selection>, Error> {
    postgres_probe::with_client(request, trust, deadline, move |client| Box::pin(async move {
        let schema = postgres_schema::lock_and_inspect(client).await.map_err(Error::Schema)?;
        if schema.store_epoch != epoch { return Err(Error::Epoch); }
        let row = client.query_one("SELECT generation,version_id FROM td_mta.config_selection WHERE singleton=true", &[]).await.map_err(|_| Error::Database)?;
        selection(&row)
    })).map_err(Error::Connection)?
}
