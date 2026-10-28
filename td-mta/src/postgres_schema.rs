//! Explicit operator schema-ledger preparation; no serving path selects it.
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

const MIGRATIONS: &[&str] = &[
    include_str!("postgres_schema.sql"),
    include_str!("postgres_materials.sql"),
    include_str!("postgres_mail.sql"),
    include_str!("postgres_devices.sql"),
    include_str!("postgres_configuration.sql"),
    include_str!("postgres_accounts.sql"),
    include_str!("postgres_selection.sql"),
    include_str!("postgres_flags.sql"),
    include_str!("postgres_flag_batches.sql"),
    include_str!("postgres_uploads.sql"),
    include_str!("postgres_drafts.sql"),
    include_str!("postgres_mailbox_create.sql"),
    include_str!("postgres_receiving.sql"),
    include_str!("postgres_submissions.sql"),
    include_str!("postgres_submission_routes.sql"),
    include_str!("postgres_route_owners.sql"),
    include_str!("postgres_attempts.sql"),
    include_str!("postgres_attempt_recovery.sql"),
    include_str!("postgres_attempt_phases.sql"),
    include_str!("postgres_attempt_acceptance.sql"),
    include_str!("postgres_attempt_refusal.sql"),
    include_str!("postgres_attempt_rcpt.sql"),
    include_str!("postgres_attempt_command.sql"),
    include_str!("postgres_queue_expiry.sql"),
    include_str!("postgres_route_health.sql"),
    include_str!("postgres_submission_cancel.sql"),
    include_str!("postgres_final_capacity.sql"),
    include_str!("postgres_runtime_barrier.sql"),
    include_str!("postgres_logs.sql"),
    include_str!("postgres_acme.sql"),
    include_str!("postgres_recovery.sql"),
    include_str!("postgres_legacy_bodies.sql"),
    include_str!("postgres_summaries.sql"),
    include_str!("postgres_mime_evidence.sql"),
    include_str!("postgres_search.sql"),
    include_str!("postgres_replica_source.sql"),
    include_str!("postgres_replica_progress.sql"),
    include_str!("postgres_replica_effects.sql"),
    include_str!("postgres_replica_mailboxes.sql"),
];
const VERSION: u32 = 39;
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
    WHERE n.nspname = 'td_mta' AND a.grantee <> c.relowner
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
/// namespace, even an empty one. This remains a preparation schema.
pub fn initialize(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
) -> Result<Receipt, Error> {
    let mut epoch = [0; 16];
    SystemEntropy::try_new()
        .and_then(|mut entropy| entropy.fill(&mut epoch))
        .map_err(|_| Error::Crypto)?;
    write_schema(
        request,
        trust,
        deadline,
        Some(StoreEpoch::from_bytes(epoch)),
    )
}

/// Explicit owner migration. Validate every prior checksum before any DDL.
/// A current compatible ledger returns its receipt without another write.
pub fn upgrade(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
) -> Result<Receipt, Error> {
    write_schema(request, trust, deadline, None)
}

fn write_schema(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    fresh_epoch: Option<StoreEpoch>,
) -> Result<Receipt, Error> {
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_schema_client(request, trust, deadline, move |client| {
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
            let (start, epoch) = if let Some(epoch) = fresh_epoch {
                let exists: bool = client.query_one("SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = 'td_mta')", &[])
                    .await.map_err(|_| Error::Database)?.try_get(0).map_err(|_| Error::Invalid)?;
                if exists {
                    return Err(Error::Exists);
                }
                (0, epoch)
            } else {
                let receipt = ledger(client, false).await?;
                if receipt.version == VERSION {
                    return Ok(receipt);
                }
                (
                    usize::try_from(receipt.version).map_err(|_| Error::Version)?,
                    receipt.store_epoch,
                )
            };
            for (index, ddl) in MIGRATIONS.iter().enumerate().skip(start) {
                let version = i32::try_from(index + 1).map_err(|_| Error::Version)?;
                let checksum = checksum(ddl)?;
                client
                    .batch_execute(ddl)
                    .await
                    .map_err(|_| Error::Database)?;
                let private: bool = client
                    .query_one(PRIVATE_ACL, &[])
                    .await
                    .map_err(|_| Error::Database)?
                    .try_get(0)
                    .map_err(|_| Error::Invalid)?;
                if !private {
                    return Err(Error::Invalid);
                }
                let rows = client.execute("INSERT INTO td_mta.schema_migrations (version, checksum, store_epoch) VALUES ($1, $2, $3)", &[&version, &checksum.as_slice(), &epoch.as_bytes().as_slice()])
                    .await.map_err(|_| Error::Database)?;
                if rows != 1 {
                    return Err(Error::Invalid);
                }
            }
            marker.store(true, Ordering::Relaxed);
            client
                .batch_execute("COMMIT")
                .await
                .map_err(|_| Error::Indeterminate)?;
            Ok(Receipt {
                version: VERSION,
                store_epoch: epoch,
            })
        })
    });
    match result {
        Ok(Ok(_)) if !attempted.load(Ordering::Relaxed) && Instant::now() >= deadline => {
            Err(Error::Connection(postgres_probe::Error::Deadline))
        }
        Ok(result) => result,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}

/// Inspect the complete ledger under a shared session migration lock. The
/// connection closes before return; its receipt grants no later SQL authority.
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
) -> Result<Receipt, Error> {
    let result = postgres_probe::with_client(request, trust, deadline, |client| {
        Box::pin(lock_and_inspect(client))
    })
    .map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}

/// The caller must retain this same session through all application SQL.
pub(crate) async fn lock_and_inspect(client: &Client) -> Result<Receipt, Error> {
    let mut timing = crate::performance::Timer::start(crate::performance::Phase::DbSchema);
    version(client).await?;
    let locked: bool = client
        .query_typed_one(
            "SELECT pg_catalog.pg_try_advisory_lock_shared($1)",
            &[(&MIGRATION_KEY, tokio_postgres::types::Type::INT8)],
        )
        .await
        .map_err(|_| Error::Database)?
        .try_get(0)
        .map_err(|_| Error::Invalid)?;
    if !locked {
        return Err(Error::Busy);
    }
    let result = ledger(client, true).await;
    if result.is_ok() {
        timing.succeeded();
    }
    result
}

// Caller has begun a transaction; its acknowledged rollback releases this
// lock together with every row lock before the next transport turn.
pub(crate) async fn lock_transaction_and_inspect(client: &Client) -> Result<Receipt, Error> {
    lock_transaction(client, false).await
}

// Owner maintenance calls this before any ledger or barrier observation.
pub(crate) async fn lock_exclusive_transaction_and_inspect(
    client: &Client,
) -> Result<Receipt, Error> {
    lock_transaction(client, true).await
}

async fn lock_transaction(client: &Client, exclusive: bool) -> Result<Receipt, Error> {
    let mut timing = crate::performance::Timer::start(crate::performance::Phase::DbSchema);
    version(client).await?;
    let locked: bool = client
        .query_typed_one(
            if exclusive {
                "SELECT pg_catalog.pg_try_advisory_xact_lock($1)"
            } else {
                "SELECT pg_catalog.pg_try_advisory_xact_lock_shared($1)"
            },
            &[(&MIGRATION_KEY, tokio_postgres::types::Type::INT8)],
        )
        .await
        .map_err(|_| Error::Database)?
        .try_get(0)
        .map_err(|_| Error::Invalid)?;
    if !locked {
        return Err(Error::Busy);
    }
    let result = ledger(client, true).await;
    if result.is_ok() {
        timing.succeeded();
    }
    result
}

async fn ledger(client: &Client, require_current: bool) -> Result<Receipt, Error> {
    let limit = MIGRATIONS.len().checked_add(1).ok_or(Error::Version)?;
    let limit = i64::try_from(limit).map_err(|_| Error::Version)?;
    let rows = client.query_typed(
        "SELECT version, checksum, store_epoch FROM td_mta.schema_migrations ORDER BY version LIMIT $1", &[(&limit, tokio_postgres::types::Type::INT8)])
        .await.map_err(|_| Error::Database)?;
    if rows.is_empty()
        || rows.len() > MIGRATIONS.len()
        || (require_current && rows.len() != MIGRATIONS.len())
    {
        return Err(Error::Version);
    }
    let mut epoch = None;
    for (index, row) in rows.iter().enumerate() {
        let version: i32 = row.try_get(0).map_err(|_| Error::Invalid)?;
        let stored: &[u8] = row.try_get(1).map_err(|_| Error::Invalid)?;
        let raw: &[u8] = row.try_get(2).map_err(|_| Error::Invalid)?;
        let current = StoreEpoch::from_bytes(raw.try_into().map_err(|_| Error::Invalid)?);
        let ddl = MIGRATIONS.get(index).ok_or(Error::Version)?;
        if version != i32::try_from(index + 1).map_err(|_| Error::Version)?
            || stored != checksum(ddl)?
            || epoch.is_some_and(|prior| prior != current)
        {
            return Err(Error::Version);
        }
        epoch = Some(current);
    }
    Ok(Receipt {
        version: u32::try_from(rows.len()).map_err(|_| Error::Version)?,
        store_epoch: epoch.ok_or(Error::Invalid)?,
    })
}

async fn version(client: &Client) -> Result<(), Error> {
    let status = postgres_probe::qualify(client)
        .await
        .map_err(Error::Connection)?;
    if status.server_version / 10_000 != 17 {
        return Err(Error::Version);
    }
    let encoding: String = client
        .query_typed_one("SELECT pg_catalog.current_setting('server_encoding')", &[])
        .await
        .map_err(|_| Error::Database)?
        .try_get(0)
        .map_err(|_| Error::Invalid)?;
    if encoding != "UTF8" {
        return Err(Error::Version);
    }
    Ok(())
}

fn checksum(ddl: &str) -> Result<[u8; 32], Error> {
    let mut digest = Provider.sha256().map_err(|_| Error::Crypto)?;
    digest.update(ddl.as_bytes()).map_err(|_| Error::Crypto)?;
    digest.finish().map_err(|_| Error::Crypto)
}
