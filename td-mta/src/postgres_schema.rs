//! Explicit operator schema-ledger preparation; no serving path selects it.
use crate::postgres_probe::with_migration_client;
use crate::postgres_probe::Client;
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
    include_str!("postgres_search_index.sql"),
    include_str!("postgres_receipt_stamps.sql"),
    include_str!("postgres_retry_floor.sql"),
    "DROP TABLE td_mta.receiving_receipts;",
    include_str!("postgres_replica_retention.sql"),
    include_str!("postgres_email_received_order.sql"),
    include_str!("postgres_sender_authentication.sql"),
];
pub const VERSION: u32 = 46;
#[path = "postgres_schema_acl.rs"]
mod acl;
// Shared by every future serving connection and explicit schema operation.
pub const MIGRATION_KEY: i64 = 0x7464_6d74_6100_0001;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Version,
    Owner,
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

pub const MAX_UPGRADE_DURATION: std::time::Duration = std::time::Duration::from_secs(3600);

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
    if request.mode != postgres_probe::Mode::Preparation {
        return Err(Error::Owner);
    }
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let maximum = if fresh_epoch.is_some() {
        postgres_probe::MAX_DURATION
    } else {
        MAX_UPGRADE_DURATION
    };
    let result = with_migration_client(request, trust, deadline, maximum, move |client| {
        version(client)?;
        client
            .batch("BEGIN; SET LOCAL synchronous_commit = on")
            .map_err(|_| Error::Database)?;
        let locked: bool = client
            .query_one(
                "SELECT pg_catalog.pg_try_advisory_xact_lock($1)",
                &[&MIGRATION_KEY],
            )
            .map_err(|_| Error::Database)?
            .try_get(0)
            .map_err(|_| Error::Invalid)?;
        if !locked {
            return Err(Error::Busy);
        }
        let (start, epoch, grants) = if let Some(epoch) = fresh_epoch {
            let exists: bool = client.query_one("SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = 'td_mta')", &[]).map_err(|_| Error::Database)?.try_get(0).map_err(|_| Error::Invalid)?;
            if exists {
                return Err(Error::Exists);
            }
            (0, epoch, Vec::new())
        } else {
            let receipt = ledger(client, false)?;
            acl::owner(client)?;
            if receipt.version == VERSION {
                client.rollback().map_err(|_| Error::Database)?;
                return Ok(receipt);
            }
            (
                usize::try_from(receipt.version).map_err(|_| Error::Version)?,
                receipt.store_epoch,
                acl::read(client)?,
            )
        };
        if fresh_epoch.is_none() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(Error::Connection(postgres_probe::Error::Deadline))?;
            let milliseconds =
                i64::try_from(remaining.as_millis().max(1)).map_err(|_| Error::Invalid)?;
            client
                    .query_one(
                        "SELECT set_config('statement_timeout', ($1::bigint)::text, true), set_config('transaction_timeout', ($1::bigint)::text, true)",
                        &[&milliseconds],
                    )
                    .map_err(|_| Error::Database)?;
        }
        for (index, ddl) in MIGRATIONS.iter().enumerate().skip(start) {
            let version = i32::try_from(index + 1).map_err(|_| Error::Version)?;
            let checksum = checksum(ddl)?;
            client.batch(ddl).map_err(|_| Error::Database)?;
            let rows = client.execute("INSERT INTO td_mta.schema_migrations (version, checksum, store_epoch) VALUES ($1, $2, $3)", &[&version, &checksum.as_slice(), &epoch.as_bytes().as_slice()]).map_err(|_| Error::Database)?;
            if rows != 1 {
                return Err(Error::Invalid);
            }
        }
        if !acl::preserves(&grants, &acl::read(client)?) {
            return Err(Error::Invalid);
        }
        marker.store(true, Ordering::Relaxed);
        client.commit().map_err(|_| Error::Indeterminate)?;
        Ok(Receipt {
            version: VERSION,
            store_epoch: epoch,
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
    let result =
        postgres_probe::with_client(request, trust, deadline, |client| client.inspect_schema())
            .map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}

/// Accept a recognized ledger prefix to plan an explicit owner migration.
pub fn plan(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
) -> Result<Receipt, Error> {
    let result = postgres_probe::with_client(request, trust, deadline, |client| {
        inspect_prefix(client, false)
    })
    .map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}

/// The caller must retain this same session through all application SQL.
pub(crate) fn lock_and_inspect(client: &Client) -> Result<Receipt, Error> {
    inspect_prefix(client, true)
}

fn inspect_prefix(client: &Client, require_current: bool) -> Result<Receipt, Error> {
    let mut timing = crate::performance::Timer::start(crate::performance::Phase::DbSchema);
    version(client)?;
    let locked: bool = client
        .query_one(
            "SELECT pg_catalog.pg_try_advisory_lock_shared($1)",
            &[&MIGRATION_KEY],
        )
        .map_err(|_| Error::Database)?
        .try_get(0)
        .map_err(|_| Error::Invalid)?;
    if !locked {
        return Err(Error::Busy);
    }
    let result = ledger(client, require_current);
    if result.is_ok() {
        timing.succeeded();
    }
    result
}

// Caller has begun a transaction; its acknowledged rollback releases this
// lock together with every row lock before the next transport turn.
pub(crate) fn lock_transaction_and_inspect(client: &Client) -> Result<Receipt, Error> {
    lock_transaction(client, false)
}

// Owner maintenance calls this before any ledger or barrier observation.
pub(crate) fn lock_exclusive_transaction_and_inspect(client: &Client) -> Result<Receipt, Error> {
    lock_transaction(client, true)
}

fn lock_transaction(client: &Client, exclusive: bool) -> Result<Receipt, Error> {
    let mut timing = crate::performance::Timer::start(crate::performance::Phase::DbSchema);
    version(client)?;
    let locked: bool = client
        .query_one(
            if exclusive {
                "SELECT pg_catalog.pg_try_advisory_xact_lock($1)"
            } else {
                "SELECT pg_catalog.pg_try_advisory_xact_lock_shared($1)"
            },
            &[&MIGRATION_KEY],
        )
        .map_err(|_| Error::Database)?
        .try_get(0)
        .map_err(|_| Error::Invalid)?;
    if !locked {
        return Err(Error::Busy);
    }
    let result = ledger(client, true);
    if result.is_ok() {
        timing.succeeded();
    }
    result
}

fn ledger(client: &Client, require_current: bool) -> Result<Receipt, Error> {
    let limit = MIGRATIONS.len().checked_add(1).ok_or(Error::Version)?;
    let limit = i64::try_from(limit).map_err(|_| Error::Version)?;
    let rows = client.query(
        "SELECT version, checksum, store_epoch FROM td_mta.schema_migrations ORDER BY version LIMIT $1", &[&limit]).map_err(|_| Error::Database)?;
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

fn version(client: &Client) -> Result<(), Error> {
    let status = postgres_probe::qualify(client).map_err(Error::Connection)?;
    if status.server_version / 10_000 != 17 {
        return Err(Error::Version);
    }
    let encoding: String = client
        .query_one("SELECT pg_catalog.current_setting('server_encoding')", &[])
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

#[cfg(test)]
mod restore_drill_tests {
    use super::*;

    #[test]
    fn recovery_verifier_pins_every_immutable_migration() -> Result<(), Box<dyn std::error::Error>>
    {
        // The host helper can travel without this crate. Check its compact pins
        // against the same byte strings and digest function that write the ledger.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../builder/src/install_mta_backup/restore_migrations.sha256");
        let pins = std::fs::read_to_string(path)?;
        let mut hashes = pins.lines();
        assert_eq!(MIGRATIONS.len(), VERSION as usize);
        for (index, ddl) in MIGRATIONS.iter().enumerate() {
            let digest = checksum(ddl).map_err(|_| "cannot compute migration checksum")?;
            let mut expected = [0; 64];
            crate::wire::hex(&digest, &mut expected)
                .map_err(|_| "cannot encode migration checksum")?;
            assert_eq!(
                hashes.next(),
                Some(std::str::from_utf8(&expected)?),
                "migration {}",
                index + 1
            );
        }
        assert_eq!(hashes.next(), None, "unexpected restore verifier digest");
        Ok(())
    }
}
