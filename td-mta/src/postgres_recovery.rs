//! Explicit recovery release after external fencing; never starts a service.
use crate::{
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalChecks {
    InstancesAndSmtpFencedAndConfigurationReviewed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Maintenance(postgres_maintenance::Error),
    Input,
    Epoch,
    BarrierRequired,
    ActiveCredentials,
    Capacity,
    Integrity,
    Database,
    /// Wait for settlement and inspect the retained new epoch; never replace it.
    Indeterminate,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Plan {
    pub old_epoch: StoreEpoch,
    pub new_epoch: StoreEpoch,
    pub utc_ms: i64,
    pub maximum_rows: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Report {
    pub epoch: StoreEpoch,
    pub quarantined_recipients: u64,
    pub completed_submissions: u64,
}

/// The caller retains both epochs before publication and attests external fences.
/// Every restored device must already be revoked. A fresh device may be created
/// after release. Existing pending deliveries become terminal OutcomeUnknown;
/// no automatic resend or evidence of acceptance is invented.
pub fn release(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    plan: Plan,
    _checks: ExternalChecks,
) -> Result<Report, Error> {
    if request.mode != Mode::Preparation
        || plan.old_epoch == plan.new_epoch
        || plan.new_epoch.as_bytes() == &[0; 16]
        || plan.utc_ms < 0
        || !(1..=1_000_000).contains(&plan.maximum_rows)
    {
        return Err(Error::Input);
    }
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_schema_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            client.batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_| Error::Database)?;
            let schema = postgres_schema::lock_exclusive_transaction_and_inspect(client)
                .await
                .map_err(Error::Schema)?;
            if schema.store_epoch != plan.old_epoch {
                return Err(Error::Epoch);
            }
            postgres_maintenance::owner(client)
                .await
                .map_err(Error::Maintenance)?;
            if !postgres_maintenance::blocked(client)
                .await
                .map_err(Error::Maintenance)?
            {
                return Err(Error::BarrierRequired);
            }
            let active: bool = client
                .query_one(
                    "SELECT EXISTS(SELECT 1 FROM td_mta.devices WHERE revoked_at IS NULL)",
                    &[],
                )
                .await
                .map_err(|_| Error::Database)?
                .try_get(0)
                .map_err(|_| Error::Integrity)?;
            if active {
                return Err(Error::ActiveCredentials);
            }
            let limit = i64::from(plan.maximum_rows) + 1;
            // Bound each rewritten relation before issuing any mutation.
            for table in [
                "accounts",
                "recipients",
                "submissions",
                "submission_attempts",
                "route_owners",
                "attempt_final_reservations",
            ] {
                let query =
                    format!("SELECT count(*) FROM (SELECT 1 FROM td_mta.{table} LIMIT $1) bounded");
                let count: i64 = client
                    .query_one(&query, &[&limit])
                    .await
                    .map_err(|_| Error::Database)?
                    .try_get(0)
                    .map_err(|_| Error::Integrity)?;
                if count >= limit {
                    return Err(Error::Capacity);
                }
            }
            let invalid: bool = client.query_one("SELECT EXISTS(SELECT 1 FROM td_mta.recipients r JOIN td_mta.submissions s ON s.account=r.account AND s.id=r.submission_id WHERE (r.state IN (1,2,3) OR (r.state=7 AND r.next_attempt_at IS NOT NULL)) AND (s.completed_at IS NOT NULL OR s.notification<>0)) OR EXISTS(SELECT 1 FROM td_mta.accounts WHERE sequence=decode(repeat('00',8),'hex') OR floor>sequence)", &[]).await.map_err(|_| Error::Database)?.try_get(0).map_err(|_| Error::Integrity)?;
            if invalid {
                return Err(Error::Integrity);
            }
            let row = client.query_one("WITH quarantined AS (UPDATE td_mta.recipients SET state=7,uncertain=1,phase=CASE WHEN attempt_count=0 THEN 0 ELSE 4 END,next_attempt_at=NULL,reason=10,diagnostic='Restored backup: delivery outcome unknown; automatic resend disabled' WHERE state IN (1,2,3) OR (state=7 AND next_attempt_at IS NOT NULL) RETURNING account,submission_id), completed AS (UPDATE td_mta.submissions s SET completed_at=$1,notification=1 WHERE EXISTS(SELECT 1 FROM quarantined q WHERE q.account=s.account AND q.submission_id=s.id) RETURNING 1) SELECT (SELECT count(*) FROM quarantined),(SELECT count(*) FROM completed)", &[&plan.utc_ms]).await.map_err(|_| Error::Database)?;
            let quarantined_recipients =
                u64::try_from(row.try_get::<_, i64>(0).map_err(|_| Error::Integrity)?)
                    .map_err(|_| Error::Integrity)?;
            let completed_submissions =
                u64::try_from(row.try_get::<_, i64>(1).map_err(|_| Error::Integrity)?)
                    .map_err(|_| Error::Integrity)?;
            client.batch_execute("UPDATE td_mta.submission_attempts SET active=false WHERE active; UPDATE td_mta.route_owners SET owner_boot=NULL,operation=NULL,expires_at=NULL WHERE owner_boot IS NOT NULL; DELETE FROM td_mta.attempt_final_reservations; UPDATE td_mta.accounts SET floor=sequence; UPDATE td_mta.queue_expiry_progress SET published_at=NULL WHERE singleton=true").await.map_err(|_| Error::Database)?;
            let changed = client
                .execute(
                    "UPDATE td_mta.schema_migrations SET store_epoch=$1",
                    &[&plan.new_epoch.as_bytes().as_slice()],
                )
                .await
                .map_err(|_| Error::Database)?;
            if changed != u64::from(schema.version) {
                return Err(Error::Integrity);
            }
            if client.execute("UPDATE td_mta.runtime_barrier SET blocked=false WHERE singleton=true AND blocked=true", &[]).await.map_err(|_| Error::Database)? != 1 { return Err(Error::Integrity); }
            marker.store(true, Ordering::Relaxed);
            client
                .batch_execute("COMMIT")
                .await
                .map_err(|_| Error::Indeterminate)?;
            Ok(Report {
                epoch: plan.new_epoch,
                quarantined_recipients,
                completed_submissions,
            })
        })
    });
    match result {
        Ok(result) => result,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}
