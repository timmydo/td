//! Owner-bound replica mode, separate from source authentication and copy progress.
use crate::{
    bounded::TextBuffer,
    config::endpoint::{Origin, MAX_URI_BYTES},
    ids::StoreEpoch,
    postgres_maintenance,
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
use td_crypto::TrustStore;
use tokio_postgres::{types::Type, Client};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Owner,
    Epoch,
    Invalid,
    Capacity,
    Occupied,
    Conflict,
    Integrity,
    Database,
    Indeterminate,
}

/// Passive validated source identity, never a device or publication credential.
#[derive(Eq, PartialEq)]
pub struct Source {
    origin: String,
    epoch: StoreEpoch,
}
impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReplicaSource(<redacted>)")
    }
}
impl Source {
    pub fn new(origin: &str, epoch: StoreEpoch) -> Result<Self, Error> {
        let parsed = Origin::parse(origin).map_err(|_| Error::Invalid)?;
        let mut bytes = [0; MAX_URI_BYTES];
        let mut out = TextBuffer::new(&mut bytes);
        parsed
            .write_canonical(&mut out)
            .map_err(|_| Error::Capacity)?;
        let canonical = out.as_str().map_err(|_| Error::Invalid)?;
        let mut origin = String::new();
        origin
            .try_reserve_exact(canonical.len())
            .map_err(|_| Error::Capacity)?;
        origin.push_str(canonical);
        Ok(Self { origin, epoch })
    }
    pub fn origin(&self) -> &str {
        &self.origin
    }
    pub fn epoch(&self) -> StoreEpoch {
        self.epoch
    }
}

pub(crate) async fn read(client: &Client) -> Result<Option<Source>, Error> {
    let rows = client
        .query_typed(
            "SELECT singleton,origin,source_epoch,protocol FROM td_mta.replica_source LIMIT 2",
            &[],
        )
        .await
        .map_err(|_| Error::Database)?;
    if rows.len() > 1 {
        return Err(Error::Integrity);
    }
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    if !row.try_get::<_, bool>(0).map_err(|_| Error::Integrity)?
        || row.try_get::<_, i32>(3).map_err(|_| Error::Integrity)? != 1
    {
        return Err(Error::Integrity);
    }
    let origin: &str = row.try_get(1).map_err(|_| Error::Integrity)?;
    let epoch: &[u8] = row.try_get(2).map_err(|_| Error::Integrity)?;
    let source = Source::new(
        origin,
        StoreEpoch::from_bytes(epoch.try_into().map_err(|_| Error::Integrity)?),
    )
    .map_err(|_| Error::Integrity)?;
    if source.origin() != origin {
        return Err(Error::Integrity);
    }
    Ok(Some(source))
}

/// Historical mode observation. Serving must check this under its startup lock
/// before listeners; this result alone never authorizes a runtime profile.
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
) -> Result<Option<Source>, Error> {
    if request.mode != Mode::Preparation {
        return Err(Error::Owner);
    }
    let result=postgres_probe::with_mutation_client(request,trust,deadline,move |client| {
        Box::pin(async move {
            let receipt=postgres_schema::lock_and_inspect(client).await.map_err(Error::Schema)?;
            if receipt.store_epoch!=epoch {return Err(Error::Epoch);}
            client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_|Error::Database)?;
            postgres_maintenance::owner(client).await.map_err(|_|Error::Owner)?;
            let source=read(client).await?;
            client.batch_execute("ROLLBACK").await.map_err(|_| Error::Database)?;
            Ok(source)
        })
    }).map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}

/// Bind an empty, unselected database. The exclusive migration lock excludes
/// every admitted runtime/owner operation; matching retries never change mode.
pub fn initialize(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    local_epoch: StoreEpoch,
    source: &Source,
) -> Result<(), Error> {
    if request.mode != Mode::Preparation {
        return Err(Error::Owner);
    }
    if source.epoch() == local_epoch {
        return Err(Error::Epoch);
    }
    let source = Source::new(source.origin(), source.epoch())?;
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_mutation_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            client.batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_|Error::Database)?;
            let receipt = postgres_schema::lock_exclusive_transaction_and_inspect(client)
                .await
                .map_err(Error::Schema)?;
            if receipt.store_epoch != local_epoch {
                return Err(Error::Epoch);
            }
            postgres_maintenance::owner(client)
                .await
                .map_err(|_| Error::Owner)?;
            if let Some(existing) = read(client).await? {
                if existing != source {
                    return Err(Error::Conflict);
                }
                client
                    .batch_execute("ROLLBACK")
                    .await
                    .map_err(|_| Error::Database)?;
                return Ok(());
            }
            let occupied:bool=client.query_typed_one("SELECT EXISTS(SELECT 1 FROM td_mta.accounts) OR EXISTS(SELECT 1 FROM td_mta.config_selection WHERE version_id IS NOT NULL) OR EXISTS(SELECT 1 FROM td_mta.acme_profiles)",&[]).await.map_err(|_|Error::Database)?.try_get(0).map_err(|_|Error::Integrity)?;
            if occupied {
                return Err(Error::Occupied);
            }
            if client.execute_typed("INSERT INTO td_mta.replica_source(singleton,origin,source_epoch,protocol) VALUES(true,$1,$2,1)",&[(&source.origin(),Type::TEXT),(&source.epoch().as_bytes().as_slice(),Type::BYTEA)]).await.map_err(|_|Error::Database)?!=1 {return Err(Error::Integrity);}
            marker.store(true, Ordering::Relaxed);
            client
                .batch_execute("COMMIT")
                .await
                .map_err(|_| Error::Indeterminate)?;
            Ok(())
        })
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn source_identity_is_canonical_and_contains_no_credentials_or_path() {
        let epoch = StoreEpoch::from_bytes([7; 16]);
        let source = Source::new("https://MAIL.example.test:443", epoch).unwrap();
        assert_eq!(source.origin(), "https://mail.example.test");
        assert_eq!(source.epoch(), epoch);
        assert_eq!(
            source,
            Source::new("https://mail.example.test", epoch).unwrap()
        );
        assert_ne!(
            source,
            Source::new("https://mail.example.test:8443", epoch).unwrap()
        );
        assert_ne!(
            source,
            Source::new("https://mail.example.test", StoreEpoch::from_bytes([8; 16])).unwrap()
        );
        for invalid in [
            "http://mail.example.test",
            "https://user:password@mail.example.test",
            "https://mail.example.test/path",
            "https://mail.example.test?token=secret",
            "https://mail.example.test#fragment",
        ] {
            assert_eq!(Source::new(invalid, epoch).unwrap_err(), Error::Invalid);
        }
        assert_eq!(format!("{source:?}"), "ReplicaSource(<redacted>)");
    }
}
