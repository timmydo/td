//! Owner-side maintenance entry and observation; recovery owns release.
use crate::postgres_probe::Client;
use crate::{
    ids::StoreEpoch,
    postgres_probe::{self, Mode, Request},
    postgres_schema,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};

/// Caller attestation, not a database proof or a substitute for external fencing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalFence {
    InstancesAndSmtpEgressStopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Owner,
    Epoch,
    Integrity,
    Database,
    /// Inspect or repeat with the same epoch; never infer rollback.
    Indeterminate,
}

pub(crate) fn owner(client: &Client) -> Result<(), Error> {
    let row = client.query_one("SELECT current_user=session_user AND n.nspowner=r.oid AND (SELECT count(*)=2 AND bool_and(c.relowner=r.oid) FROM pg_catalog.pg_class c WHERE c.relnamespace=n.oid AND c.relname IN ('schema_migrations','runtime_barrier') AND c.relkind='r' AND c.relpersistence='p' AND NOT c.relrowsecurity AND NOT c.relforcerowsecurity) FROM pg_catalog.pg_namespace n JOIN pg_catalog.pg_roles r ON r.rolname=current_user WHERE n.nspname='td_mta'", &[]).map_err(|_| Error::Database)?;
    if !row.try_get::<_, bool>(0).map_err(|_| Error::Integrity)? {
        return Err(Error::Owner);
    }
    Ok(())
}

pub(crate) fn blocked(client: &Client) -> Result<bool, Error> {
    let rows = client
        .query(
            "SELECT singleton,blocked FROM td_mta.runtime_barrier LIMIT 2",
            &[],
        )
        .map_err(|_| Error::Database)?;
    if rows.len() != 1 {
        return Err(Error::Integrity);
    }
    let row = rows.first().ok_or(Error::Integrity)?;
    if !row.try_get::<_, bool>(0).map_err(|_| Error::Integrity)? {
        return Err(Error::Integrity);
    }
    row.try_get(1).map_err(|_| Error::Integrity)
}

/// A historical observation under the migration lock, never serving authority.
pub fn inspect(request: &Request<'_>, deadline: Instant, epoch: StoreEpoch) -> Result<bool, Error> {
    if request.mode != Mode::Preparation {
        return Err(Error::Owner);
    }
    let result = postgres_probe::with_schema_client(request, deadline, move |client| {
        let receipt = postgres_schema::lock_and_inspect(client).map_err(Error::Schema)?;
        if receipt.store_epoch != epoch {
            return Err(Error::Epoch);
        }
        client
            .batch("BEGIN ISOLATION LEVEL READ COMMITTED READ ONLY")
            .map_err(|_| Error::Database)?;
        owner(client)?;
        let state = blocked(client)?;
        client.rollback().map_err(|_| Error::Database)?;
        Ok(state)
    })
    .map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}

/// Enter maintenance after externally fencing all instances and SMTP egress.
/// Repetition at this epoch is monotone. Recovery release must renew the epoch
/// atomically; postgres_recovery owns the explicit release protocol.
pub fn enter(
    request: &Request<'_>,
    deadline: Instant,
    epoch: StoreEpoch,
    _fence: ExternalFence,
) -> Result<(), Error> {
    if request.mode != Mode::Preparation {
        return Err(Error::Owner);
    }
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_schema_client(request, deadline, move |client| {
        client.batch("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=10000").map_err(|_| Error::Database)?;
        let receipt = postgres_schema::lock_exclusive_transaction_and_inspect(client)
            .map_err(Error::Schema)?;
        if receipt.store_epoch != epoch {
            return Err(Error::Epoch);
        }
        owner(client)?;
        if blocked(client)? {
            client.rollback().map_err(|_| Error::Database)?;
            return Ok(());
        }
        let rows=client.execute("UPDATE td_mta.runtime_barrier SET blocked=true WHERE singleton=true AND blocked=false", &[]).map_err(|_| Error::Database)?;
        if rows != 1 {
            return Err(Error::Integrity);
        }
        marker.store(true, Ordering::Relaxed);
        client.commit().map_err(|_| Error::Indeterminate)?;
        Ok(())
    });
    match result {
        Ok(Ok(())) if !attempted.load(Ordering::Relaxed) && Instant::now() >= deadline => {
            Err(Error::Connection(postgres_probe::Error::Deadline))
        }
        Ok(result) => result,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}
