//! Retire completed submissions once their retention ends, and reclaim
//! expired upload leases on the same relay turns.
use crate::postgres_probe::Client;
use crate::{
    postgres_probe::{self, Request},
    postgres_publication as publication,
    postgres_route_owners::{self as owners, Claim},
    queue_clock::RetentionTime,
};
use std::time::Instant;
use td_crypto::TrustStore;

/// QUEUE.md: completed records stay 30 days after completion.
pub const RETENTION_MS: i64 = 30 * 86_400_000;
/// One retirement transaction stays far inside the statement timeout and
/// the account lock stays short: blob chunks cascade in one statement.
pub const MAX_SUBMISSIONS: i64 = 64;
pub const MAX_RECIPIENTS: i64 = 2048;
pub const MAX_BYTES: i64 = 64 * 1024 * 1024;
/// The database clock must still agree with the relay's stable observation.
pub const CLOCK_SLACK_MS: i64 = 60_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Owner(owners::Error),
    Policy,
    Integrity,
    Capacity,
    Database,
    Clock,
}

/// Independent upkeep operations under one account's current relay claim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {
    Retire,
    UploadLeases,
    AbandonedOwners,
    IdleAttempts,
}

impl Step {
    /// Reclaim one bounded batch and return its committed count. The next
    /// pass can retry after a lost COMMIT. A stale owner or clock refuses
    /// every step before it changes any rows.
    pub fn run(
        self,
        request: &Request<'_>,
        trust: &TrustStore,
        deadline: Instant,
        claim: &Claim,
        generation: u64,
        stable_at: impl Into<RetentionTime>,
    ) -> Result<u64, Error> {
        let stable_at = stable_at.into();
        let claim = claim.clone();
        postgres_probe::with_mutation_client(request, trust, deadline, move |client| {
            let now = lock(client, &claim, generation, stable_at)?;
            let account = claim.binding().account();
            let account = account.as_bytes().as_slice();
            let count = match self {
                Self::Retire => retire_locked(client, account, now)?,
                Self::UploadLeases => crate::postgres_uploads::reclaim(client, account, now)
                    .map_err(|error| match error {
                        crate::postgres_uploads::Error::Database => Error::Database,
                        crate::postgres_uploads::Error::Capacity => Error::Capacity,
                        _ => Error::Integrity,
                    })?,
                Self::AbandonedOwners => {
                    owners::reclaim_abandoned(client, &claim, now).map_err(Error::Owner)?
                }
                Self::IdleAttempts => {
                    crate::postgres_attempt_reclamation::reclaim(client, account, None, now)
                        .map_err(|error| match error {
                            crate::postgres_attempts::Error::Database => Error::Database,
                            _ => Error::Integrity,
                        })?
                }
            };
            if count > 0 {
                owners::check_locked(client, &claim, generation).map_err(Error::Owner)?;
                client.commit().map_err(|_| Error::Database)?;
            }
            Ok(count)
        })
        .map_err(Error::Connection)?
    }
}

/// Begin the relay's account transaction: the account row FOR UPDATE under
/// a current owner fence, returning database time near the stable sample.
fn lock(
    client: &postgres_probe::Client,
    claim: &Claim,
    generation: u64,
    stable_at: RetentionTime,
) -> Result<i64, Error> {
    if !owners::begin(client, claim, true, true).map_err(Error::Owner)? {
        return Err(Error::Policy);
    }
    let now = owners::check_locked(client, claim, generation).map_err(Error::Owner)?;
    if now.abs_diff(stable_at.observed) > CLOCK_SLACK_MS.unsigned_abs() {
        return Err(Error::Clock);
    }
    Ok(now.min(stable_at.cutoff))
}

/// Whether a candidate fits after earlier ones, with `recipients` and
/// `bytes` totals including it. The first always fits: one submission holds
/// at most 1000 recipients and one 32 MiB blob.
fn fits(taken: usize, recipients: i64, bytes: i64) -> bool {
    taken == 0 || (recipients <= MAX_RECIPIENTS && bytes <= MAX_BYTES)
}

fn retire_locked(client: &Client, account: &[u8], now: i64) -> Result<u64, Error> {
    let cutoff = now.checked_sub(RETENTION_MS).ok_or(Error::Integrity)?;
    let candidates = client.query(concat!("SELECT s.id,s.recipient_count,b.length FROM td_mta.submissions s JOIN td_mta.blobs b ON b.account=s.account AND b.id=s.transmitted_blob_id WHERE s.account=$1 AND s.completed_at<$2 AND s.notification<>1 AND NOT EXISTS(SELECT 1 FROM td_mta.submission_attempts a WHERE a.account=s.account AND a.submission_id=s.id AND (a.active OR ",include_str!("postgres_pending_route_reset.sql")," OR EXISTS(SELECT 1 FROM td_mta.attempt_final_reservations f WHERE f.account=a.account AND f.attempt_id=a.attempt_id))) ORDER BY s.completed_at,s.id LIMIT $3 FOR UPDATE OF s"), &[&account, &cutoff, &MAX_SUBMISSIONS]).map_err(|_| Error::Database)?;
    if candidates.is_empty() {
        return Ok(0);
    }
    let budget = td_alloc::Budget::new(64 * 1024).map_err(|_| Error::Capacity)?;
    let mut submissions: td_alloc::BudgetVec<&[u8]> =
        td_alloc::BudgetVec::with_capacity(&budget, candidates.len())
            .map_err(|_| Error::Capacity)?;
    let (mut recipients, mut bytes) = (0i64, 0i64);
    for row in &candidates {
        let id: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
        let count: i64 = row.try_get(1).map_err(|_| Error::Integrity)?;
        let length: i64 = row.try_get(2).map_err(|_| Error::Integrity)?;
        if id.len() != 16 || !(1..=1000).contains(&count) || length < 0 {
            return Err(Error::Integrity);
        }
        recipients = recipients.checked_add(count).ok_or(Error::Integrity)?;
        bytes = bytes.checked_add(length).ok_or(Error::Integrity)?;
        if !fits(submissions.len(), recipients, bytes) {
            break;
        }
        submissions.try_push(id).map_err(|_| Error::Capacity)?;
    }
    // Receipts carry no foreign key to their submission or attempt, so each
    // is removed explicitly; route-health receipts of retired attempts would
    // otherwise linger until that route publishes again (section 123).
    // Attempt IDs stay server-side: an account may hold 1024 of them, more
    // rows than the connection's frame budget admits.
    client.execute("WITH attempts AS (SELECT attempt_id FROM td_mta.submission_attempts WHERE account=$1 AND submission_id=ANY($2::bytea[])), phase AS (DELETE FROM td_mta.attempt_phase_receipts WHERE account=$1 AND attempt_id IN (SELECT attempt_id FROM attempts)), acceptance AS (DELETE FROM td_mta.attempt_acceptance_receipts WHERE account=$1 AND attempt_id IN (SELECT attempt_id FROM attempts)), refusal AS (DELETE FROM td_mta.attempt_refusal_receipts WHERE account=$1 AND attempt_id IN (SELECT attempt_id FROM attempts)), rcpt AS (DELETE FROM td_mta.attempt_rcpt_receipts WHERE account=$1 AND attempt_id IN (SELECT attempt_id FROM attempts)), command AS (DELETE FROM td_mta.attempt_command_receipts WHERE account=$1 AND attempt_id IN (SELECT attempt_id FROM attempts)), recovery AS (DELETE FROM td_mta.attempt_recovery_receipts WHERE account=$1 AND attempt_id IN (SELECT attempt_id FROM attempts)), health AS (DELETE FROM td_mta.route_health_receipts WHERE account=$1 AND action IN (1,3) AND operation IN (SELECT attempt_id FROM attempts)), submitted AS (DELETE FROM td_mta.submission_receipts WHERE account=$1 AND submission_id=ANY($2::bytea[])), canceled AS (DELETE FROM td_mta.submission_cancel_receipts WHERE account=$1 AND submission_id=ANY($2::bytea[])), expired AS (DELETE FROM td_mta.submission_expiry_receipts WHERE account=$1 AND submission_id=ANY($2::bytea[])) DELETE FROM td_mta.submission_attempts WHERE account=$1 AND submission_id=ANY($2::bytea[])", &[&account, &submissions.as_slice()]).map_err(|_| Error::Database)?;
    // submission_routes cascades; recipients are deferred to COMMIT.
    let blob_rows = client.query("WITH removed AS (DELETE FROM td_mta.recipients WHERE account=$1 AND submission_id=ANY($2::bytea[])) DELETE FROM td_mta.submissions WHERE account=$1 AND id=ANY($2::bytea[]) RETURNING transmitted_blob_id", &[&account, &submissions.as_slice()]).map_err(|_| Error::Database)?;
    if blob_rows.len() != submissions.len() {
        return Err(Error::Integrity);
    }
    let mut blobs: td_alloc::BudgetVec<&[u8]> =
        td_alloc::BudgetVec::with_capacity(&budget, blob_rows.len())
            .map_err(|_| Error::Capacity)?;
    for row in &blob_rows {
        let id: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
        blobs.try_push(id).map_err(|_| Error::Capacity)?;
    }
    // A blob another submission still transmits stays queued; one that an
    // Email or lease still reaches stays stored.
    let queued: i64 = client.query_one("SELECT coalesce(sum(b.length),0)::bigint FROM td_mta.blobs b WHERE b.account=$1 AND b.id IN (SELECT DISTINCT unnest($2::bytea[])) AND NOT EXISTS(SELECT 1 FROM td_mta.submissions s WHERE s.account=b.account AND s.transmitted_blob_id=b.id)", &[&account, &blobs.as_slice()]).map_err(|_| Error::Database)?.try_get(0).map_err(|_| Error::Integrity)?;
    let refused = |error| match error {
        publication::Error::Database => Error::Database,
        _ => Error::Integrity,
    };
    let (bytes, count) =
        publication::retire_blobs(client, account, blobs.as_slice()).map_err(refused)?;
    let retired = i64::try_from(submissions.len()).map_err(|_| Error::Integrity)?;
    publication::release(client, account, [bytes, count, 0, queued, retired]).map_err(refused)?;
    let state = client
        .query_one(
            "SELECT sequence FROM td_mta.accounts WHERE id=$1",
            &[&account],
        )
        .map_err(|_| Error::Database)?;
    let old: &[u8] = state.try_get(0).map_err(|_| Error::Integrity)?;
    let old = u64::from_be_bytes(old.try_into().map_err(|_| Error::Integrity)?);
    let next =
        crate::postgres_sequence::next(client, account, old).map_err(|error| match error {
            crate::postgres_sequence::Error::Capacity => Error::Capacity,
            crate::postgres_sequence::Error::Integrity => Error::Integrity,
            crate::postgres_sequence::Error::Database => Error::Database,
        })?;
    let changed = next.to_be_bytes();
    // Destroyed rows advance every state string and keep each ID from reuse.
    let written = client.execute("INSERT INTO td_mta.changes(account,sequence,operation,kind,action,object) SELECT $1,$3,ordinality-1,5,3,id FROM unnest($2::bytea[]) WITH ORDINALITY q(id,ordinality)", &[&account, &submissions.as_slice(), &changed.as_slice()]).map_err(|_| Error::Database)?;
    if written != submissions.len() as u64 {
        return Err(Error::Integrity);
    }
    if client
        .execute(
            "UPDATE td_mta.accounts SET sequence=$2 WHERE id=$1",
            &[&account, &changed.as_slice()],
        )
        .map_err(|_| Error::Database)?
        != 1
    {
        return Err(Error::Integrity);
    }
    u64::try_from(retired).map_err(|_| Error::Integrity)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn batches_stop_at_recipient_and_byte_bounds_but_admit_one() {
        assert!(fits(0, 1000, 32 * 1024 * 1024));
        assert!(fits(0, MAX_RECIPIENTS + 1, MAX_BYTES + 1));
        assert!(fits(1, MAX_RECIPIENTS, MAX_BYTES));
        assert!(!fits(1, MAX_RECIPIENTS + 1, 0));
        assert!(!fits(1, 2, MAX_BYTES + 1));
    }
}
