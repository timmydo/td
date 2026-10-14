//! Inert schema-ledger preparation. No command or serving path selects it.
use crate::{
    ids::StoreEpoch,
    postgres_probe::{self, Request},
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

const DDL: &str = include_str!("postgres_schema.sql");
const VERSION: i32 = 1;
const PRIVATE_ACL: &str = "SELECT NOT EXISTS (
    SELECT 1 FROM pg_catalog.pg_namespace n,
    LATERAL pg_catalog.aclexplode(COALESCE(n.nspacl,
        pg_catalog.acldefault('n', n.nspowner))) a
    WHERE n.nspname = 'td_mta' AND a.grantee <> n.nspowner
) AND NOT EXISTS (
    SELECT 1 FROM pg_catalog.pg_class c
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace,
    LATERAL pg_catalog.aclexplode(COALESCE(c.relacl,
        pg_catalog.acldefault('r', c.relowner))) a
    WHERE n.nspname = 'td_mta' AND c.relname = 'schema_migrations'
        AND a.grantee <> c.relowner
)";
// Shared by every future serving connection and explicit schema operation.
pub const MIGRATION_KEY: i64 = 0x7464_6d74_6100_0001;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Version,
    Busy,
    Exists,
    Invalid,
    Database,
    Crypto,
    /// COMMIT may have reached the server. Never replay initialization blindly.
    Indeterminate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Receipt {
    pub version: u32,
    pub store_epoch: StoreEpoch,
}

/// Requires an explicit schema-owner credential. Refuses any existing td_mta
/// namespace, even an empty one. This ledger alone is not a serving schema.
pub fn initialize(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
) -> Result<Receipt, Error> {
    let checksum = checksum()?;
    let mut epoch = [0; 16];
    SystemEntropy::try_new()
        .and_then(|mut entropy| entropy.fill(&mut epoch))
        .map_err(|_| Error::Crypto)?;
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            version(client).await?;
            client
                .batch_execute("BEGIN; SET LOCAL synchronous_commit = on")
                .await
                .map_err(|_| Error::Database)?;
            let locked: bool = client
                .query_one(
                    "SELECT pg_catalog.pg_try_advisory_xact_lock($1)",
                    &[&MIGRATION_KEY],
                )
                .await
                .map_err(|_| Error::Database)?
                .try_get(0)
                .map_err(|_| Error::Invalid)?;
            if !locked {
                return Err(Error::Busy);
            }
            let exists: bool = client.query_one("SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = 'td_mta')", &[])
            .await.map_err(|_| Error::Database)?.try_get(0).map_err(|_| Error::Invalid)?;
            if exists {
                return Err(Error::Exists);
            }
            client
                .batch_execute(DDL)
                .await
                .map_err(|_| Error::Database)?;
            // Refuse inherited default ACLs granting another role access. The
            // runtime role and its narrowly scoped grants are not installed yet.
            let private: bool = client
                .query_one(PRIVATE_ACL, &[])
                .await
                .map_err(|_| Error::Database)?
                .try_get(0)
                .map_err(|_| Error::Invalid)?;
            if !private {
                return Err(Error::Invalid);
            }
            let rows = client.execute("INSERT INTO td_mta.schema_migrations (version, checksum, store_epoch) VALUES ($1, $2, $3)", &[&VERSION, &checksum.as_slice(), &epoch.as_slice()])
            .await.map_err(|_| Error::Database)?;
            if rows != 1 {
                return Err(Error::Invalid);
            }
            marker.store(true, Ordering::Relaxed);
            client
                .batch_execute("COMMIT")
                .await
                .map_err(|_| Error::Indeterminate)?;
            Ok(Receipt {
                version: VERSION as u32,
                store_epoch: StoreEpoch::from_bytes(epoch),
            })
        })
    });
    match result {
        Ok(result) => result,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}

/// Inspect one ledger while retaining a shared session-level migration lock.
/// The connection and lock close before this returns; it grants no authority
/// to perform later application SQL on another connection.
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
) -> Result<Receipt, Error> {
    let checksum = checksum()?;
    let result = postgres_probe::with_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            version(client).await?;
            let locked: bool = client
                .query_one(
                    "SELECT pg_catalog.pg_try_advisory_lock_shared($1)",
                    &[&MIGRATION_KEY],
                )
                .await
                .map_err(|_| Error::Database)?
                .try_get(0)
                .map_err(|_| Error::Invalid)?;
            if !locked {
                return Err(Error::Busy);
            }
            let rows = client
                .query(
                    "SELECT version, checksum, store_epoch FROM td_mta.schema_migrations LIMIT 2",
                    &[],
                )
                .await
                .map_err(|_| Error::Database)?;
            if rows.len() != 1 {
                return Err(Error::Invalid);
            }
            let row = rows.first().ok_or(Error::Invalid)?;
            let version: i32 = row.try_get(0).map_err(|_| Error::Invalid)?;
            let stored: &[u8] = row.try_get(1).map_err(|_| Error::Invalid)?;
            let epoch: &[u8] = row.try_get(2).map_err(|_| Error::Invalid)?;
            if version != VERSION || stored != checksum {
                return Err(Error::Version);
            }
            Ok(Receipt {
                version: VERSION as u32,
                store_epoch: StoreEpoch::from_bytes(epoch.try_into().map_err(|_| Error::Invalid)?),
            })
        })
    })
    .map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}

async fn version(client: &Client) -> Result<(), Error> {
    let status = postgres_probe::qualify(client)
        .await
        .map_err(Error::Connection)?;
    if status.server_version / 10_000 != 17 {
        return Err(Error::Version);
    }
    Ok(())
}

fn checksum() -> Result<[u8; 32], Error> {
    let mut digest = Provider.sha256().map_err(|_| Error::Crypto)?;
    digest.update(DDL.as_bytes()).map_err(|_| Error::Crypto)?;
    digest.finish().map_err(|_| Error::Crypto)
}
