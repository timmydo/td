//! Reclaim retired delivery attempts and their receipts at attempt
//! publication (POSTGRES.md section 218).
use crate::postgres_attempts::Error;
use tokio_postgres::Client;

/// Attempts one publication reclaims: more than it adds, so a full account
/// drains, and few enough that the statement stays short under the lock.
pub const RECLAIM_ATTEMPTS: i64 = 64;
const _: () = assert!(RECLAIM_ATTEMPTS > 1 && RECLAIM_ATTEMPTS <= 1024);

/// The caller holds the account row FOR UPDATE, owns `route` and has no
/// pending operation; `timestamp` is the locked database time. Deletes the
/// oldest retired attempts without a final reservation, on the publishing route at any
/// age and elsewhere once prepared before the route-health receipt
/// margin, with every receipt keyed by them. Submission-keyed receipts,
/// recipients and sequences stay. Maintenance passes no publishing route.
/// Live owners retain final acceptance until their health reset is recorded.
pub(crate) async fn reclaim(
    client: &Client,
    account: &[u8],
    route: Option<&str>,
    timestamp: i64,
) -> Result<u64, Error> {
    let cutoff = timestamp
        .checked_sub(crate::postgres_route_health::RECEIPT_RETENTION_MS)
        .ok_or(Error::Integrity)?;
    client.execute(concat!("WITH reclaimed AS (SELECT a.attempt_id FROM td_mta.submission_attempts a WHERE a.account=$1 AND NOT a.active AND (a.route_name=$2 OR a.prepared_at<$3) AND NOT ", include_str!("postgres_pending_route_reset.sql"), " AND NOT EXISTS(SELECT 1 FROM td_mta.attempt_final_reservations f WHERE f.account=a.account AND f.attempt_id=a.attempt_id) ORDER BY a.prepared_at,a.attempt_id LIMIT $4 FOR UPDATE), phase AS (DELETE FROM td_mta.attempt_phase_receipts WHERE account=$1 AND attempt_id IN (SELECT attempt_id FROM reclaimed)), rcpt AS (DELETE FROM td_mta.attempt_rcpt_receipts WHERE account=$1 AND attempt_id IN (SELECT attempt_id FROM reclaimed)), command AS (DELETE FROM td_mta.attempt_command_receipts WHERE account=$1 AND attempt_id IN (SELECT attempt_id FROM reclaimed)), acceptance AS (DELETE FROM td_mta.attempt_acceptance_receipts WHERE account=$1 AND attempt_id IN (SELECT attempt_id FROM reclaimed)), refusal AS (DELETE FROM td_mta.attempt_refusal_receipts WHERE account=$1 AND attempt_id IN (SELECT attempt_id FROM reclaimed)), recovery AS (DELETE FROM td_mta.attempt_recovery_receipts WHERE account=$1 AND attempt_id IN (SELECT attempt_id FROM reclaimed)), health AS (DELETE FROM td_mta.route_health_receipts WHERE account=$1 AND action IN (1,3) AND operation IN (SELECT attempt_id FROM reclaimed)) DELETE FROM td_mta.submission_attempts WHERE account=$1 AND attempt_id IN (SELECT attempt_id FROM reclaimed)"), &[&account, &route, &cutoff, &RECLAIM_ATTEMPTS]).await.map_err(|_| Error::Database)
}
