//! Verified transmitted bytes for an active prepared attempt, not SMTP authority.
use super::{Dispatch, Error, Outcome};
use crate::{
    ids::{AttemptId, BlobId, SubmissionId},
    ports::{self, BlobReader},
    postgres_body::{self, BlobMetadata},
    postgres_probe::{self, Request},
    postgres_route_owners as owners, postgres_schema,
};
use std::time::Instant;
use td_crypto::TrustStore;
use tokio_postgres::Client;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadError {
    Attempt(Error),
    Body(postgres_body::Error),
    Output(ports::Error),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BodyMetadata {
    pub attempt: AttemptId,
    pub submission: SubmissionId,
    pub blob: BlobMetadata,
}

/// Caller reserves connection/body-work capacity. Verification and callback
/// replay share one immutable snapshot and the original absolute deadline.
/// Fresh owner/attempt admission precedes the snapshot and follows verification.
/// No account or route locks cross the callback. Metadata is passive: current
/// transport ownership and durable phase fences still precede any SMTP output.
pub fn with_body<T>(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Dispatch,
    maximum: u64,
    read: impl FnOnce(BodyMetadata, &mut dyn BlobReader) -> Result<T, ports::Error>,
) -> Result<T, ReadError> {
    if maximum > postgres_body::MAX_BODY_BYTES {
        return Err(ReadError::Body(postgres_body::Error::Capacity));
    }
    admit(request, trust, deadline, value).map_err(ReadError::Attempt)?;
    postgres_probe::with_body_worker(request, trust, deadline, |worker| {
        let selected = value.clone();
        let metadata = worker
            .run(|client| Box::pin(snapshot(client, selected, maximum)))
            .map_err(|error| ReadError::Attempt(Error::Connection(error)))??;
        postgres_body::with_verified_pin(worker, metadata.blob, deadline, |body| {
            Ok(replay(
                body,
                || admit(request, trust, deadline, value).map_err(ReadError::Attempt),
                |body| read(metadata, body),
            ))
        })
        .map_err(ReadError::Body)?
    })
    .map_err(|error| ReadError::Attempt(Error::Connection(error)))?
}

fn replay<T>(
    body: &mut crate::store_fs::PinnedBlob<'_, '_>,
    admit: impl FnOnce() -> Result<(), ReadError>,
    read: impl FnOnce(&mut dyn BlobReader) -> Result<T, ports::Error>,
) -> Result<T, ReadError> {
    admit()?;
    body.check_deadline().map_err(ReadError::Output)?;
    read(body).map_err(ReadError::Output)
}

fn admit(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Dispatch,
) -> Result<(), Error> {
    let value = value.clone();
    postgres_probe::with_mailbox_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            if !owners::begin(client, &value.claim, true, false)
                .await
                .map_err(Error::Owner)?
            {
                return Err(Error::Policy);
            }
            owners::check_locked(client, &value.claim, value.generation)
                .await
                .map_err(Error::Owner)?;
            active(client, &value).await?;
            owners::check_locked(client, &value.claim, value.generation)
                .await
                .map_err(Error::Owner)?;
            Ok(())
        })
    })
    .map_err(Error::Connection)?
}
async fn active(client: &Client, value: &Dispatch) -> Result<Outcome, Error> {
    let outcome = super::receipt(client, value).await?.ok_or(Error::Missing)?;
    let binding = value.claim.binding();
    let rows=client.query("SELECT active,owner_boot,owner_operation,generation,route_name,destination FROM td_mta.submission_attempts WHERE account=$1 AND attempt_id=$2", &[&binding.account().as_bytes().as_slice(),&value.attempt.as_bytes().as_slice()]).await.map_err(|_|Error::Database)?;
    let row = rows.first().ok_or(Error::Missing)?;
    let active: bool = row.try_get(0).map_err(|_| Error::Integrity)?;
    let boot: &[u8] = row.try_get(1).map_err(|_| Error::Integrity)?;
    let operation: &[u8] = row.try_get(2).map_err(|_| Error::Integrity)?;
    let generation = super::number(row.try_get(3).map_err(|_| Error::Integrity)?)?;
    let route: &str = row.try_get(4).map_err(|_| Error::Integrity)?;
    let destination: &[u8] = row.try_get(5).map_err(|_| Error::Integrity)?;
    if !active {
        return Err(Error::Missing);
    }
    if boot != value.claim.boot().as_bytes()
        || operation != value.claim.operation().as_bytes()
        || generation != value.generation
        || route != binding.route().name()
        || destination != binding.route().destination()
    {
        return Err(Error::Conflict);
    }
    Ok(outcome)
}
async fn snapshot(
    client: &Client,
    value: Dispatch,
    maximum: u64,
) -> Result<BodyMetadata, ReadError> {
    let binding = value.claim.binding();
    let schema = postgres_schema::lock_and_inspect(client)
        .await
        .map_err(|e| ReadError::Body(postgres_body::Error::Schema(e)))?;
    if schema.store_epoch != binding.epoch() {
        return Err(ReadError::Body(postgres_body::Error::Epoch));
    }
    client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_|ReadError::Attempt(Error::Database))?;
    active(client, &value).await.map_err(ReadError::Attempt)?;
    let rows=client.query("SELECT a.sequence,a.floor,b.length,b.digest,b.created_at,b.chunk_count,b.changed,(SELECT count(*) FROM td_mta.blob_chunks c WHERE c.account=b.account AND c.blob_id=b.id),s.transmitted_blob_id,s.changed FROM td_mta.accounts a JOIN td_mta.submissions s ON s.account=a.id JOIN td_mta.submission_routes r ON r.account=s.account AND r.submission_id=s.id LEFT JOIN td_mta.blobs b ON b.account=s.account AND b.id=s.transmitted_blob_id WHERE a.id=$1 AND s.id=$2 AND s.completed_at IS NULL AND r.route_name=$3 AND r.destination=$4", &[&binding.account().as_bytes().as_slice(),&value.submission.as_bytes().as_slice(),&binding.route().name(),&binding.route().destination().as_slice()]).await.map_err(|_|ReadError::Attempt(Error::Database))?;
    let row = rows.first().ok_or(ReadError::Attempt(Error::Missing))?;
    let bytes: &[u8] = row
        .try_get(8)
        .map_err(|_| ReadError::Attempt(Error::Integrity))?;
    let blob = BlobId::from_bytes(
        bytes
            .try_into()
            .map_err(|_| ReadError::Attempt(Error::Integrity))?,
    );
    let metadata =
        postgres_body::blob_metadata(row, binding.account(), binding.epoch(), blob, maximum)
            .map_err(ReadError::Body)?;
    let changed = super::number(
        row.try_get(9)
            .map_err(|_| ReadError::Attempt(Error::Integrity))?,
    )
    .map_err(ReadError::Attempt)?;
    if changed > metadata.identity.committed_sequence.number() {
        return Err(ReadError::Attempt(Error::Integrity));
    }
    Ok(BodyMetadata {
        attempt: value.attempt,
        submission: value.submission,
        blob: metadata,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        format::row::BlobRow,
        store_fs::{PinnedBlob, VerifiedSource},
    };
    use std::cell::Cell;

    struct Source<'a>(&'a Cell<Option<ports::Error>>);
    impl BlobReader for Source<'_> {
        fn len(&self) -> u64 {
            0
        }
        fn read_at(&mut self, _: u64, _: &mut [u8]) -> Result<usize, ports::Error> {
            Ok(0)
        }
    }
    impl VerifiedSource for Source<'_> {
        fn check(&mut self) -> Result<(), ports::Error> {
            self.0.get().map_or(Ok(()), Err)
        }
    }
    #[test]
    fn snapshot_failure_during_admission_prevents_callback() -> Result<(), ports::Error> {
        for error in [
            ports::Error::Deadline,
            ports::Error::Io {
                kind: std::io::ErrorKind::ConnectionAborted,
                os_code: None,
            },
        ] {
            let failure = Cell::new(None);
            let mut source = Source(&failure);
            let mut pin = PinnedBlob::from_verified(
                &mut source,
                BlobId::from_bytes([1; 16]),
                BlobRow {
                    length: 0,
                    digest: [0; 32],
                    created_at: 0,
                },
            )?;
            let called = Cell::new(false);
            assert_eq!(
                replay(
                    &mut pin,
                    || {
                        failure.set(Some(error));
                        Ok(())
                    },
                    |_| {
                        called.set(true);
                        Ok(())
                    }
                ),
                Err(ReadError::Output(error))
            );
            assert!(!called.get());
        }
        Ok(())
    }
}
