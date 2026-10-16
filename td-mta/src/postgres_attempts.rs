//! Atomic bounded attempt publication; scheduling and SMTP remain inactive.
use crate::{
    ids::{AttemptId, SubmissionId},
    postgres_probe::{self, Request},
    postgres_route_owners::{self as owners, Claim},
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

#[path = "postgres_attempt_body.rs"]
mod body;
pub use body::{with_body, BodyMetadata, ReadError};

#[path = "postgres_attempt_envelope.rs"]
mod envelope_reads;
pub use envelope_reads::{envelope, Envelope, Recipient};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Owner(owners::Error),
    RouteHealth(crate::postgres_route_health::Error),
    Policy,
    Missing,
    NotDue,
    Busy,
    Conflict,
    Capacity,
    Integrity,
    Crypto,
    Database,
    Indeterminate,
}
#[derive(Clone)]
pub struct Dispatch {
    claim: Claim,
    generation: u64,
    submission: SubmissionId,
    attempt: AttemptId,
    digest: [u8; 32],
    retry: bool,
}
impl Dispatch {
    pub fn attempt(&self) -> AttemptId {
        self.attempt
    }
}
/// Historical publication evidence, never continuing permission to transmit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Outcome {
    pub attempt: AttemptId,
    pub submission: SubmissionId,
    pub prepared_at: i64,
    pub ordinals: Vec<u32>,
    pub old_sequence: u64,
    pub new_sequence: u64,
}
pub fn prepare(
    claim: &Claim,
    generation: u64,
    submission: SubmissionId,
) -> Result<Dispatch, Error> {
    prepare_mode(claim, generation, submission, false)
}
/// Prepare a due-retry batch. Runtime callers must first enforce queue clock,
/// route backoff and repair guards; this storage operation is not a scheduler.
pub fn retry(claim: &Claim, generation: u64, submission: SubmissionId) -> Result<Dispatch, Error> {
    prepare_mode(claim, generation, submission, true)
}
fn prepare_mode(
    claim: &Claim,
    generation: u64,
    submission: SubmissionId,
    retry: bool,
) -> Result<Dispatch, Error> {
    if generation == 0 {
        return Err(Error::Integrity);
    }
    let mut attempt = [0; 16];
    SystemEntropy::try_new()
        .map_err(|_| Error::Crypto)?
        .fill(&mut attempt)
        .map_err(|_| Error::Crypto)?;
    let binding = claim.binding();
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    for field in [
        if retry {
            b"td-mta retry dispatch v1".as_slice()
        } else {
            b"td-mta first dispatch v1".as_slice()
        },
        binding.epoch().as_bytes().as_slice(),
        binding.account().as_bytes().as_slice(),
        claim.operation().as_bytes().as_slice(),
        claim.boot().as_bytes().as_slice(),
        &generation.to_be_bytes(),
        submission.as_bytes().as_slice(),
        &attempt,
    ] {
        hash.update(&(field.len() as u64).to_be_bytes())
            .map_err(|_| Error::Crypto)?;
        hash.update(field).map_err(|_| Error::Crypto)?;
    }
    Ok(Dispatch {
        claim: claim.clone(),
        generation,
        submission,
        attempt: AttemptId::from_bytes(attempt),
        digest: hash.finish().map_err(|_| Error::Crypto)?,
        retry,
    })
}
fn number(bytes: &[u8]) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    ))
}
async fn receipt(client: &Client, value: &Dispatch) -> Result<Option<Outcome>, Error> {
    let rows=client.query("SELECT request_digest,submission_id,prepared_at,ordinals,old_sequence,new_sequence FROM td_mta.submission_attempts WHERE account=$1 AND attempt_id=$2", &[&value.claim.binding().account().as_bytes().as_slice(), &value.attempt.as_bytes().as_slice()]).await.map_err(|_|Error::Database)?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let digest: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
    if digest != value.digest {
        return Err(Error::Conflict);
    }
    let submission: &[u8] = row.try_get(1).map_err(|_| Error::Integrity)?;
    let encoded: &[u8] = row.try_get(3).map_err(|_| Error::Integrity)?;
    let old = number(row.try_get(4).map_err(|_| Error::Integrity)?)?;
    let new = number(row.try_get(5).map_err(|_| Error::Integrity)?)?;
    if submission != value.submission.as_bytes()
        || encoded.is_empty()
        || encoded.len() > 400
        || !encoded.len().is_multiple_of(4)
        || old.checked_add(1) != Some(new)
    {
        return Err(Error::Integrity);
    }
    let mut ordinals = Vec::new();
    ordinals
        .try_reserve_exact(encoded.len() / 4)
        .map_err(|_| Error::Capacity)?;
    for chunk in encoded.as_chunks::<4>().0 {
        let ordinal = u32::from_be_bytes(*chunk);
        if ordinal >= 1000 || ordinals.last().is_some_and(|last| *last >= ordinal) {
            return Err(Error::Integrity);
        }
        ordinals.push(ordinal);
    }
    Ok(Some(Outcome {
        attempt: value.attempt,
        submission: value.submission,
        prepared_at: row.try_get(2).map_err(|_| Error::Integrity)?,
        ordinals,
        old_sequence: old,
        new_sequence: new,
    }))
}
async fn active_receipt(client: &Client, value: &Dispatch) -> Result<Outcome, Error> {
    let outcome = receipt(client, value).await?.ok_or(Error::Missing)?;
    let binding = value.claim.binding();
    let rows=client.query("SELECT active,owner_boot,owner_operation,generation,route_name,destination FROM td_mta.submission_attempts WHERE account=$1 AND attempt_id=$2", &[&binding.account().as_bytes().as_slice(),&value.attempt.as_bytes().as_slice()]).await.map_err(|_|Error::Database)?;
    let row = rows.first().ok_or(Error::Missing)?;
    let active: bool = row.try_get(0).map_err(|_| Error::Integrity)?;
    let boot: &[u8] = row.try_get(1).map_err(|_| Error::Integrity)?;
    let operation: &[u8] = row.try_get(2).map_err(|_| Error::Integrity)?;
    let generation = number(row.try_get(3).map_err(|_| Error::Integrity)?)?;
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
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Dispatch,
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
/// Select at most 100 fresh Queued or due retry recipients in ordinal order,
/// according to the retained preparation. Historical replay authorizes no send.
pub fn publish(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Dispatch,
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
            crate::postgres_route_health::admit_locked(client, &value.claim, timestamp)
                .await
                .map_err(Error::RouteHealth)?;
            let binding = value.claim.binding();
            let account = binding.account();
            let account = account.as_bytes().as_slice();
            let submission = value.submission.as_bytes().as_slice();
            let active:bool=client.query_one("SELECT EXISTS(SELECT 1 FROM td_mta.submission_attempts WHERE account=$1 AND route_name=$2 AND active)", &[&account,&binding.route().name()]).await.map_err(|_|Error::Database)?.try_get(0).map_err(|_|Error::Integrity)?;
            if active {
                return Err(Error::Busy);
            }
            let count:i64=client.query_one("SELECT count(*) FROM (SELECT 1 FROM td_mta.submission_attempts WHERE account=$1 LIMIT 1024) attempts", &[&account]).await.map_err(|_|Error::Database)?.try_get(0).map_err(|_|Error::Integrity)?;
            if count >= 1024 {
                return Err(Error::Capacity);
            }
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
            let next = old.checked_add(1).ok_or(Error::Capacity)?;
            let rows=client.query("SELECT s.send_at,s.expires_at,s.recipient_count,s.completed_at,s.changed FROM td_mta.submissions s JOIN td_mta.submission_routes r ON r.account=s.account AND r.submission_id=s.id WHERE s.account=$1 AND s.id=$2 AND r.route_name=$3 AND r.destination=$4 FOR UPDATE OF s", &[&account,&submission,&binding.route().name(),&binding.route().destination().as_slice()]).await.map_err(|_|Error::Database)?;
            let row = rows.first().ok_or(Error::Missing)?;
            let send_at: i64 = row.try_get(0).map_err(|_| Error::Integrity)?;
            let expires_at: i64 = row.try_get(1).map_err(|_| Error::Integrity)?;
            let recipient_count: i64 = row.try_get(2).map_err(|_| Error::Integrity)?;
            let completed: Option<i64> = row.try_get(3).map_err(|_| Error::Integrity)?;
            if !(1..=1000).contains(&recipient_count)
                || number(row.try_get(4).map_err(|_| Error::Integrity)?)? > old
            {
                return Err(Error::Integrity);
            }
            if completed.is_some() || timestamp < send_at || timestamp >= expires_at {
                return Err(Error::NotDue);
            }
            let query = if value.retry {
                "SELECT ordinal,uncertain,attempt_id,attempt_count,last_attempt_at,phase,rcpt_reply,data_reply,reason,changed,state,next_attempt_at FROM td_mta.recipients WHERE account=$1 AND submission_id=$2 AND state IN (3,7) AND next_attempt_at<=$3 ORDER BY ordinal LIMIT 100 FOR UPDATE"
            } else {
                "SELECT ordinal,uncertain,attempt_id,attempt_count,last_attempt_at,phase,rcpt_reply,data_reply,reason,changed,state,next_attempt_at FROM td_mta.recipients WHERE account=$1 AND submission_id=$2 AND state=1 AND next_attempt_at<=$3 ORDER BY ordinal LIMIT 100 FOR UPDATE"
            };
            let rows = client
                .query(query, &[&account, &submission, &timestamp])
                .await
                .map_err(|_| Error::Database)?;
            if rows.is_empty() {
                return Err(Error::NotDue);
            }
            let mut ordinals = Vec::new();
            let mut encoded = Vec::new();
            let mut selected = Vec::new();
            ordinals
                .try_reserve_exact(rows.len())
                .map_err(|_| Error::Capacity)?;
            encoded
                .try_reserve_exact(rows.len() * 4)
                .map_err(|_| Error::Capacity)?;
            selected
                .try_reserve_exact(rows.len())
                .map_err(|_| Error::Capacity)?;
            for row in &rows {
                let ordinal: i64 = row.try_get(0).map_err(|_| Error::Integrity)?;
                let uncertain: i64 = row.try_get(1).map_err(|_| Error::Integrity)?;
                let attempt: Option<&[u8]> = row.try_get(2).map_err(|_| Error::Integrity)?;
                let count: i64 = row.try_get(3).map_err(|_| Error::Integrity)?;
                let last: Option<i64> = row.try_get(4).map_err(|_| Error::Integrity)?;
                let phase: i64 = row.try_get(5).map_err(|_| Error::Integrity)?;
                let rcpt: Option<&str> = row.try_get(6).map_err(|_| Error::Integrity)?;
                let data: Option<&str> = row.try_get(7).map_err(|_| Error::Integrity)?;
                let reason: i64 = row.try_get(8).map_err(|_| Error::Integrity)?;
                if ordinal < 0
                    || ordinal >= recipient_count
                    || number(row.try_get(9).map_err(|_| Error::Integrity)?)? > old
                {
                    return Err(Error::Integrity);
                }
                if value.retry {
                    let state: i64 = row.try_get(10).map_err(|_| Error::Integrity)?;
                    let due: i64 = row.try_get(11).map_err(|_| Error::Integrity)?;
                    let last = last.ok_or(Error::Integrity)?;
                    if !matches!((state, uncertain), (3, 0) | (7, 1))
                        || attempt.is_none_or(|id| id.len() != 16)
                        || count <= 0
                        || phase != 4
                        || last
                            .checked_add(300_000)
                            .is_none_or(|minimum| due < minimum)
                        || !(matches!(reason, 1 | 3 | 4 | 5 | 8) || (reason == 9 && uncertain == 1))
                        || (reason == 1
                            && crate::recipient_sweep::reply_class(data.or(rcpt)) != Some(4))
                    {
                        return Err(Error::Integrity);
                    }
                    if u32::try_from(count)
                        .ok()
                        .and_then(|count| count.checked_add(1))
                        .is_none()
                    {
                        return Err(Error::Capacity);
                    }
                } else if uncertain != 0
                    || attempt.is_some()
                    || count != 0
                    || last.is_some()
                    || phase != 0
                    || rcpt.is_some()
                    || data.is_some()
                    || reason != 0
                {
                    return Err(Error::Integrity);
                }
                let ordinal32 = u32::try_from(ordinal).map_err(|_| Error::Integrity)?;
                ordinals.push(ordinal32);
                encoded.extend_from_slice(&ordinal32.to_be_bytes());
                selected.push(ordinal);
            }
            let changed = next.to_be_bytes();
            let changed = changed.as_slice();
            if client.execute("UPDATE td_mta.recipients SET state=2,attempt_id=$3,attempt_count=attempt_count+1,last_attempt_at=$4,phase=1,next_attempt_at=NULL,reason=0,diagnostic='',changed=$5 WHERE account=$1 AND submission_id=$2 AND ordinal=ANY($6)", &[&account,&submission,&value.attempt.as_bytes().as_slice(),&timestamp,&changed,&selected]).await.map_err(|_|Error::Database)?!=ordinals.len() as u64 {return Err(Error::Integrity)}
            if client
                .execute(
                    "UPDATE td_mta.submissions SET changed=$3 WHERE account=$1 AND id=$2",
                    &[&account, &submission, &changed],
                )
                .await
                .map_err(|_| Error::Database)?
                != 1
            {
                return Err(Error::Integrity);
            }
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
            client.execute("INSERT INTO td_mta.submission_attempts VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,true)", &[&account,&value.attempt.as_bytes().as_slice(),&value.digest.as_slice(),&submission,&binding.route().name(),&binding.route().destination().as_slice(),&value.claim.boot().as_bytes().as_slice(),&value.claim.operation().as_bytes().as_slice(),&value.generation.to_be_bytes().as_slice(),&timestamp,&encoded,&old.to_be_bytes().as_slice(),&changed]).await.map_err(|_|Error::Database)?;
            marker.store(true, Ordering::Relaxed);
            client
                .batch_execute("COMMIT")
                .await
                .map_err(|_| Error::Indeterminate)?;
            Ok(Outcome {
                attempt: value.attempt,
                submission: value.submission,
                prepared_at: timestamp,
                ordinals,
                old_sequence: old,
                new_sequence: next,
            })
        })
    });
    match result {
        Ok(value) => value,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}
