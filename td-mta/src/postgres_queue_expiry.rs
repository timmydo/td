//! Clock-confirmed, fenced, bounded expiry; no scheduler or transport activation.
use crate::{
    ids::{SubmissionExpiryId, SubmissionId},
    ports::{Clock, Time},
    postgres_probe::{self, Request},
    postgres_route_owners::{self as owners, Claim},
    queue_clock::{self, Guard},
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};
use td_crypto::{Crypto, Digest, Entropy, Provider, SystemEntropy, TrustStore};
use tokio_postgres::Client;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Owner(owners::Error),
    Clock(queue_clock::Error),
    Policy,
    Missing,
    NotDue,
    RateLimited,
    Conflict,
    Capacity,
    Integrity,
    Crypto,
    Database,
    Indeterminate,
}
fn capacity(error: crate::postgres_final_capacity::Error) -> Error {
    match error {
        crate::postgres_final_capacity::Error::Capacity => Error::Capacity,
        crate::postgres_final_capacity::Error::Integrity => Error::Integrity,
        crate::postgres_final_capacity::Error::Database => Error::Database,
    }
}

#[derive(Clone)]
pub struct Expiry {
    claim: Claim,
    generation: u64,
    submission: SubmissionId,
    operation: SubmissionExpiryId,
    digest: [u8; 32],
}
impl Expiry {
    pub fn operation(&self) -> SubmissionExpiryId {
        self.operation
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Outcome {
    pub submission: SubmissionId,
    pub recipients: u32,
    pub cutoff_at: i64,
    pub published_at: i64,
    pub old_sequence: u64,
    pub new_sequence: u64,
}
pub fn prepare(claim: &Claim, generation: u64, submission: SubmissionId) -> Result<Expiry, Error> {
    if generation == 0 {
        return Err(Error::Integrity);
    }
    let mut operation = [0; 16];
    SystemEntropy::try_new()
        .map_err(|_| Error::Crypto)?
        .fill(&mut operation)
        .map_err(|_| Error::Crypto)?;
    let binding = claim.binding();
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    for field in [
        b"td-mta queue expiry v1".as_slice(),
        binding.epoch().as_bytes().as_slice(),
        binding.account().as_bytes().as_slice(),
        claim.operation().as_bytes().as_slice(),
        claim.boot().as_bytes().as_slice(),
        &generation.to_be_bytes(),
        submission.as_bytes().as_slice(),
        &operation,
    ] {
        hash.update(&(field.len() as u64).to_be_bytes())
            .map_err(|_| Error::Crypto)?;
        hash.update(field).map_err(|_| Error::Crypto)?;
    }
    Ok(Expiry {
        claim: claim.clone(),
        generation,
        submission,
        operation: SubmissionExpiryId::from_bytes(operation),
        digest: hash.finish().map_err(|_| Error::Crypto)?,
    })
}
fn number(bytes: &[u8]) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    ))
}
async fn receipt(client: &Client, value: &Expiry) -> Result<Option<Outcome>, Error> {
    let rows=client.query("SELECT request_digest,submission_id,recipient_count,cutoff_at,published_at,old_sequence,new_sequence FROM td_mta.submission_expiry_receipts WHERE account=$1 AND operation=$2", &[&value.claim.binding().account().as_bytes().as_slice(),&value.operation.as_bytes().as_slice()]).await.map_err(|_|Error::Database)?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let digest: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
    if digest != value.digest {
        return Err(Error::Conflict);
    }
    let submission: &[u8] = row.try_get(1).map_err(|_| Error::Integrity)?;
    let count: i64 = row.try_get(2).map_err(|_| Error::Integrity)?;
    let cutoff: i64 = row.try_get(3).map_err(|_| Error::Integrity)?;
    let published: i64 = row.try_get(4).map_err(|_| Error::Integrity)?;
    let old = number(row.try_get(5).map_err(|_| Error::Integrity)?)?;
    let new = number(row.try_get(6).map_err(|_| Error::Integrity)?)?;
    if submission != value.submission.as_bytes()
        || !(1..=100).contains(&count)
        || cutoff < 0
        || published < cutoff
        || old.checked_add(1) != Some(new)
    {
        return Err(Error::Integrity);
    }
    Ok(Some(Outcome {
        submission: value.submission,
        recipients: u32::try_from(count).map_err(|_| Error::Integrity)?,
        cutoff_at: cutoff,
        published_at: published,
        old_sequence: old,
        new_sequence: new,
    }))
}
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Expiry,
) -> Result<Option<Outcome>, Error> {
    let value = value.clone();
    postgres_probe::with_mailbox_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            owners::begin(client, &value.claim, false, false)
                .await
                .map_err(Error::Owner)?;
            receipt(client, &value).await
        })
    })
    .map_err(Error::Connection)?
}
fn sample(clock: &dyn Clock, utc_ms: i64) -> Result<Time, Error> {
    let monotonic = clock
        .sample()
        .map_err(|_| Error::Clock(queue_clock::Error::Invalid))?
        .monotonic;
    Ok(Time { utc_ms, monotonic })
}
/// Caller must close/fence the route's transport before expiry, including the
/// current owner's attempt. All expiry callers in a process share one guard.
/// Historical settlement precedes clock admission and grants no new mutation.
pub fn publish(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Expiry,
    clock: Arc<dyn Clock>,
    guard: Arc<Mutex<Guard>>,
) -> Result<Outcome, Error> {
    if let Some(outcome) = inspect(request, trust, deadline, value)? {
        return Ok(outcome);
    }
    let value = value.clone();
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_mutation_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let policy = owners::begin(client, &value.claim, true, true)
                .await
                .map_err(Error::Owner)?;
            if let Some(outcome) = receipt(client, &value).await? {
                return Ok(outcome);
            }
            if !policy {
                return Err(Error::Policy);
            }
            let timestamp = owners::check_locked(client, &value.claim, value.generation)
                .await
                .map_err(Error::Owner)?;
            let mut token = guard
                .try_lock()
                .map_err(|_| Error::Clock(queue_clock::Error::Invalid))?
                .admit_expiry(sample(clock.as_ref(), timestamp)?)
                .map_err(Error::Clock)?;
            let cutoff = token.cutoff_utc_ms();
            let progress=client.query("SELECT published_at FROM td_mta.queue_expiry_progress WHERE singleton FOR UPDATE", &[]).await.map_err(|_|Error::Database)?;
            let previous: Option<i64> = progress
                .first()
                .ok_or(Error::Integrity)?
                .try_get(0)
                .map_err(|_| Error::Integrity)?;
            if previous.is_some_and(|previous| {
                previous < 0 || previous.checked_add(1000).is_none_or(|next| cutoff < next)
            }) {
                return Err(Error::RateLimited);
            }
            let binding = value.claim.binding();
            let account = binding.account();
            let account = account.as_bytes().as_slice();
            let submission = value.submission.as_bytes().as_slice();
            let state = client
                .query_one(
                    "SELECT sequence,floor FROM td_mta.accounts WHERE id=$1",
                    &[&account],
                )
                .await
                .map_err(|_| Error::Database)?;
            let old = number(state.try_get(0).map_err(|_| Error::Integrity)?)?;
            if number(state.try_get(1).map_err(|_| Error::Integrity)?)? > old {
                return Err(Error::Integrity);
            }
            let rows=client.query("SELECT s.expires_at,s.completed_at,s.recipient_count,s.changed FROM td_mta.submissions s JOIN td_mta.submission_routes r ON r.account=s.account AND r.submission_id=s.id WHERE s.account=$1 AND s.id=$2 AND r.route_name=$3 AND r.destination=$4 FOR UPDATE OF s", &[&account,&submission,&binding.route().name(),&binding.route().destination().as_slice()]).await.map_err(|_|Error::Database)?;
            let row = rows.first().ok_or(Error::Missing)?;
            let expires: i64 = row.try_get(0).map_err(|_| Error::Integrity)?;
            let completed: Option<i64> = row.try_get(1).map_err(|_| Error::Integrity)?;
            let expected: i64 = row.try_get(2).map_err(|_| Error::Integrity)?;
            if !(1..=1000).contains(&expected)
                || number(row.try_get(3).map_err(|_| Error::Integrity)?)? > old
            {
                return Err(Error::Integrity);
            }
            if completed.is_some() || expires > cutoff {
                return Err(Error::NotDue);
            }
            let reserved =
                crate::postgres_final_capacity::expiry_subset(client, account, submission)
                    .await
                    .map_err(capacity)?;
            let rows = if let Some(crate::postgres_final_capacity::Subset { attempt, ordinals }) =
                &reserved
            {
                let rows = client.query("SELECT ordinal,state,uncertain,attempt_id,attempt_count,last_attempt_at,phase,next_attempt_at,rcpt_reply,data_reply,reason,changed FROM td_mta.recipients WHERE account=$1 AND submission_id=$2 AND attempt_id=$3 AND state=2 AND phase=3 ORDER BY ordinal LIMIT 101 FOR UPDATE", &[&account,&submission,attempt]).await.map_err(|_|Error::Database)?;
                if rows.len() != ordinals.len() {
                    return Err(Error::Integrity);
                }
                rows
            } else {
                client.query("SELECT ordinal,state,uncertain,attempt_id,attempt_count,last_attempt_at,phase,next_attempt_at,rcpt_reply,data_reply,reason,changed FROM td_mta.recipients WHERE account=$1 AND submission_id=$2 AND (state IN (1,2,3) OR (state=7 AND next_attempt_at IS NOT NULL)) ORDER BY ordinal LIMIT 100 FOR UPDATE", &[&account,&submission]).await.map_err(|_|Error::Database)?
            };
            if rows.is_empty() {
                return Err(Error::NotDue);
            }
            let mut selected = Vec::new();
            selected
                .try_reserve_exact(rows.len())
                .map_err(|_| Error::Capacity)?;
            for row in &rows {
                let ordinal: i64 = row.try_get(0).map_err(|_| Error::Integrity)?;
                let state: i64 = row.try_get(1).map_err(|_| Error::Integrity)?;
                let uncertain: i64 = row.try_get(2).map_err(|_| Error::Integrity)?;
                let attempt: Option<&[u8]> = row.try_get(3).map_err(|_| Error::Integrity)?;
                let count: i64 = row.try_get(4).map_err(|_| Error::Integrity)?;
                let last: Option<i64> = row.try_get(5).map_err(|_| Error::Integrity)?;
                let phase: i64 = row.try_get(6).map_err(|_| Error::Integrity)?;
                let due: Option<i64> = row.try_get(7).map_err(|_| Error::Integrity)?;
                let rcpt: Option<&str> = row.try_get(8).map_err(|_| Error::Integrity)?;
                let data: Option<&str> = row.try_get(9).map_err(|_| Error::Integrity)?;
                let reason: i64 = row.try_get(10).map_err(|_| Error::Integrity)?;
                if ordinal < 0
                    || ordinal >= expected
                    || !(0..=1).contains(&uncertain)
                    || number(row.try_get(11).map_err(|_| Error::Integrity)?)? > old
                {
                    return Err(Error::Integrity);
                }
                if state == 1 {
                    if uncertain != 0
                        || attempt.is_some()
                        || count != 0
                        || last.is_some()
                        || phase != 0
                        || due.is_none()
                        || rcpt.is_some()
                        || data.is_some()
                        || reason != 0
                    {
                        return Err(Error::Integrity);
                    }
                } else {
                    if attempt.is_none_or(|id| id.len() != 16)
                        || !(1..=i64::from(u32::MAX)).contains(&count)
                        || last.is_none_or(|last| last > cutoff)
                    {
                        return Err(Error::Integrity);
                    }
                    if state == 2 {
                        if phase == 3 && reserved.is_none() {
                            return Err(Error::Integrity);
                        }
                        if !(1..=3).contains(&phase)
                            || due.is_some()
                            || reason != 0
                            || (phase != 1
                                && (crate::recipient_sweep::reply_class(rcpt) != Some(2)
                                    || data.is_some()))
                        {
                            return Err(Error::Integrity);
                        }
                    } else if phase != 4
                        || due.is_none()
                        || (state == 3 && uncertain != 0)
                        || (state == 7 && uncertain != 1)
                        || !(matches!(reason, 1 | 3 | 4 | 5 | 8) || (reason == 9 && uncertain == 1))
                        || (reason == 1
                            && crate::recipient_sweep::reply_class(data.or(rcpt)) != Some(4))
                    {
                        return Err(Error::Integrity);
                    }
                }
                selected.push(ordinal);
            }
            if let Some(crate::postgres_final_capacity::Subset { attempt, ordinals }) = &reserved {
                if &selected != ordinals {
                    return Err(Error::Integrity);
                }
                crate::postgres_final_capacity::consume(client, account, attempt, ordinals)
                    .await
                    .map_err(capacity)?;
            }
            crate::postgres_final_capacity::admit_receipt(
                client,
                account,
                crate::postgres_final_capacity::Receipt::Expiry,
            )
            .await
            .map_err(capacity)?;
            let next = crate::postgres_sequence::next(client, account, old)
                .await
                .map_err(|error| match error {
                    crate::postgres_sequence::Error::Capacity => Error::Capacity,
                    crate::postgres_sequence::Error::Integrity => Error::Integrity,
                    crate::postgres_sequence::Error::Database => Error::Database,
                })?;
            let changed = next.to_be_bytes();
            let changed = changed.as_slice();
            if client.execute("UPDATE td_mta.recipients SET state=CASE WHEN uncertain=1 OR phase=3 THEN 7 ELSE 5 END,uncertain=CASE WHEN uncertain=1 OR phase=3 THEN 1 ELSE 0 END,phase=CASE WHEN attempt_count=0 THEN 0 ELSE 4 END,next_attempt_at=NULL,reason=6,diagnostic='',changed=$3 WHERE account=$1 AND submission_id=$2 AND ordinal=ANY($4)", &[&account,&submission,&changed,&selected]).await.map_err(|_|Error::Database)?!=selected.len() as u64 {return Err(Error::Integrity);}
            let totals=client.query_one("SELECT count(*),min(ordinal),max(ordinal),count(*) FILTER (WHERE state IN (1,2,3) OR (state=7 AND next_attempt_at IS NOT NULL)) FROM td_mta.recipients WHERE account=$1 AND submission_id=$2", &[&account,&submission]).await.map_err(|_|Error::Database)?;
            let count: i64 = totals.try_get(0).map_err(|_| Error::Integrity)?;
            let first: Option<i64> = totals.try_get(1).map_err(|_| Error::Integrity)?;
            let last: Option<i64> = totals.try_get(2).map_err(|_| Error::Integrity)?;
            let pending: i64 = totals.try_get(3).map_err(|_| Error::Integrity)?;
            if count != expected || first != Some(0) || last != count.checked_sub(1) {
                return Err(Error::Integrity);
            }
            let published = owners::check_locked(client, &value.claim, value.generation)
                .await
                .map_err(Error::Owner)?;
            guard
                .try_lock()
                .map_err(|_| Error::Clock(queue_clock::Error::Invalid))?
                .validate_expiry(&mut token, sample(clock.as_ref(), published)?)
                .map_err(Error::Clock)?;
            if client.execute("UPDATE td_mta.submissions SET changed=$3,completed_at=CASE WHEN $4 THEN $5 ELSE completed_at END,notification=CASE WHEN $4 AND notification=0 THEN 1 ELSE notification END WHERE account=$1 AND id=$2", &[&account,&submission,&changed,&(pending==0),&published]).await.map_err(|_|Error::Database)?!=1 {return Err(Error::Integrity);}
            client.execute("UPDATE td_mta.submission_attempts a SET active=false WHERE account=$1 AND submission_id=$2 AND active AND NOT EXISTS (SELECT 1 FROM td_mta.recipients r WHERE r.account=a.account AND r.submission_id=a.submission_id AND r.attempt_id=a.attempt_id AND r.state=2)", &[&account,&submission]).await.map_err(|_|Error::Database)?;
            client
                .execute(
                    "INSERT INTO td_mta.changes VALUES($1,$2,0,5,2,$3)",
                    &[&account, &changed, &submission],
                )
                .await
                .map_err(|_| Error::Database)?;
            if client
                .execute(
                    "UPDATE td_mta.accounts SET sequence=$2 WHERE id=$1",
                    &[&account, &changed],
                )
                .await
                .map_err(|_| Error::Database)?
                != 1
            {
                return Err(Error::Integrity);
            }
            let count = i64::try_from(selected.len()).map_err(|_| Error::Capacity)?;
            client.execute("INSERT INTO td_mta.submission_expiry_receipts VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)", &[&account,&value.operation.as_bytes().as_slice(),&value.digest.as_slice(),&submission,&count,&cutoff,&published,&old.to_be_bytes().as_slice(),&changed]).await.map_err(|_|Error::Database)?;
            if client
                .execute(
                    "UPDATE td_mta.queue_expiry_progress SET published_at=$1 WHERE singleton",
                    &[&published],
                )
                .await
                .map_err(|_| Error::Database)?
                != 1
            {
                return Err(Error::Integrity);
            }
            let final_time = owners::check_locked(client, &value.claim, value.generation)
                .await
                .map_err(Error::Owner)?;
            guard
                .try_lock()
                .map_err(|_| Error::Clock(queue_clock::Error::Invalid))?
                .validate_expiry(&mut token, sample(clock.as_ref(), final_time)?)
                .map_err(Error::Clock)?;
            marker.store(true, Ordering::Relaxed);
            client
                .batch_execute("COMMIT")
                .await
                .map_err(|_| Error::Indeterminate)?;
            Ok(Outcome {
                submission: value.submission,
                recipients: u32::try_from(count).map_err(|_| Error::Integrity)?,
                cutoff_at: cutoff,
                published_at: published,
                old_sequence: old,
                new_sequence: next,
            })
        })
    });
    match result {
        Ok(result) => result,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}
