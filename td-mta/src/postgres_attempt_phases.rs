//! Durable DATA phase fences. Historical receipts never authorize transport.
use crate::{
    ids::AttemptId,
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
use td_crypto::{Crypto, Digest, Provider, TrustStore};
use tokio_postgres::Client;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Owner(owners::Error),
    Policy,
    Missing,
    Integrity,
    Capacity,
    Crypto,
    Conflict,
    Database,
    Indeterminate,
}
#[derive(Clone)]
pub struct Change {
    claim: Claim,
    generation: u64,
    attempt: AttemptId,
    phase: i64,
    ordinals: Vec<i64>,
    replies: Vec<String>,
    digest: [u8; 32],
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Outcome {
    pub attempt: AttemptId,
    pub recipients: u32,
    pub old_sequence: u64,
    pub new_sequence: u64,
}
/// Supply precisely the recipients accepted by RCPT after a positive DATA
/// command. Replies are already normalized SMTP text, not local diagnostics.
pub fn body(
    claim: &Claim,
    generation: u64,
    attempt: AttemptId,
    accepted: &[(u32, &str)],
) -> Result<Change, Error> {
    prepare(claim, generation, attempt, 2, accepted.iter().copied())
}
/// Supply the entire previously committed Body subset. The caller must retain
/// final-outcome capacity and current ownership before emitting a terminator.
pub fn acceptance_possible(
    claim: &Claim,
    generation: u64,
    attempt: AttemptId,
    ordinals: &[u32],
) -> Result<Change, Error> {
    prepare(
        claim,
        generation,
        attempt,
        3,
        ordinals.iter().map(|n| (*n, "")),
    )
}
fn prepare<'a>(
    claim: &Claim,
    generation: u64,
    attempt: AttemptId,
    phase: i64,
    recipients: impl Iterator<Item = (u32, &'a str)>,
) -> Result<Change, Error> {
    if generation == 0 {
        return Err(Error::Integrity);
    }
    let mut ordinals = Vec::new();
    let mut replies = Vec::new();
    ordinals
        .try_reserve_exact(100)
        .map_err(|_| Error::Capacity)?;
    replies
        .try_reserve_exact(100)
        .map_err(|_| Error::Capacity)?;
    let binding = claim.binding();
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    for field in [
        b"td-mta attempt phase v1".as_slice(),
        binding.epoch().as_bytes().as_slice(),
        binding.account().as_bytes().as_slice(),
        claim.operation().as_bytes().as_slice(),
        claim.boot().as_bytes().as_slice(),
        &generation.to_be_bytes(),
        attempt.as_bytes().as_slice(),
        &phase.to_be_bytes(),
    ] {
        hash.update(&(field.len() as u64).to_be_bytes())
            .map_err(|_| Error::Crypto)?;
        hash.update(field).map_err(|_| Error::Crypto)?;
    }
    for (ordinal, reply) in recipients {
        let ordinal = i64::from(ordinal);
        if ordinals.len() == 100
            || ordinal >= 1000
            || ordinals.last().is_some_and(|last| *last >= ordinal)
            || (phase == 2 && !positive_reply(reply))
        {
            return Err(Error::Integrity);
        }
        let mut retained = String::new();
        retained
            .try_reserve_exact(reply.len())
            .map_err(|_| Error::Capacity)?;
        retained.push_str(reply);
        hash.update(&ordinal.to_be_bytes())
            .map_err(|_| Error::Crypto)?;
        hash.update(&(reply.len() as u64).to_be_bytes())
            .map_err(|_| Error::Crypto)?;
        hash.update(reply.as_bytes()).map_err(|_| Error::Crypto)?;
        ordinals.push(ordinal);
        replies.push(retained);
    }
    if ordinals.is_empty() {
        return Err(Error::Integrity);
    }
    Ok(Change {
        claim: claim.clone(),
        generation,
        attempt,
        phase,
        ordinals,
        replies,
        digest: hash.finish().map_err(|_| Error::Crypto)?,
    })
}
fn positive_reply(reply: &str) -> bool {
    reply.len() <= crate::format::row::MAX_SMTP_REPLY
        && crate::recipient_sweep::reply_class(Some(reply)) == Some(2)
        && !reply.chars().any(char::is_control)
}
fn number(bytes: &[u8]) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    ))
}
async fn receipt(client: &Client, value: &Change) -> Result<Option<Outcome>, Error> {
    let rows = client.query("SELECT request_digest,recipient_count,old_sequence,new_sequence FROM td_mta.attempt_phase_receipts WHERE account=$1 AND attempt_id=$2 AND phase=$3", &[&value.claim.binding().account().as_bytes().as_slice(), &value.attempt.as_bytes().as_slice(), &value.phase]).await.map_err(|_| Error::Database)?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let digest: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
    if digest != value.digest {
        return Err(Error::Conflict);
    }
    let count: i64 = row.try_get(1).map_err(|_| Error::Integrity)?;
    let old = number(row.try_get(2).map_err(|_| Error::Integrity)?)?;
    let new = number(row.try_get(3).map_err(|_| Error::Integrity)?)?;
    if usize::try_from(count).ok() != Some(value.ordinals.len()) || old.checked_add(1) != Some(new)
    {
        return Err(Error::Integrity);
    }
    Ok(Some(Outcome {
        attempt: value.attempt,
        recipients: u32::try_from(count).map_err(|_| Error::Integrity)?,
        old_sequence: old,
        new_sequence: new,
    }))
}
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Change,
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
/// Publishes only a phase, never a remote result. Unknown COMMIT requires the
/// caller to close transport; receipt settlement is historical evidence only.
pub fn publish(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Change,
) -> Result<Outcome, Error> {
    if let Some(outcome) = inspect(request, trust, deadline, value)? {
        return Ok(outcome);
    }
    let value = value.clone();
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_batch_client(request, trust, deadline, move |client| {
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
            let binding = value.claim.binding();
            let account_id = binding.account();
            let account = account_id.as_bytes().as_slice();
            let attempt = value.attempt.as_bytes().as_slice();
            let rows = client.query("SELECT submission_id,ordinals FROM td_mta.submission_attempts WHERE account=$1 AND attempt_id=$2 AND route_name=$3 AND destination=$4 AND generation=$5 AND owner_boot=$6 AND owner_operation=$7 AND active FOR UPDATE", &[&account,&attempt,&binding.route().name(),&binding.route().destination().as_slice(),&value.generation.to_be_bytes().as_slice(),&value.claim.boot().as_bytes().as_slice(),&value.claim.operation().as_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
            let row = rows.first().ok_or(Error::Missing)?;
            let submission: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
            let encoded: &[u8] = row.try_get(1).map_err(|_| Error::Integrity)?;
            if submission.len() != 16
                || encoded.is_empty()
                || encoded.len() > 400
                || !encoded.len().is_multiple_of(4)
            {
                return Err(Error::Integrity);
            }
            let mut previous = None;
            for bytes in encoded.as_chunks::<4>().0 {
                let ordinal = u32::from_be_bytes(*bytes);
                if ordinal >= 1000 || previous.is_some_and(|p| p >= ordinal) {
                    return Err(Error::Integrity);
                }
                previous = Some(ordinal);
            }
            if value.ordinals.iter().any(|n| {
                !encoded
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|b| i64::from(u32::from_be_bytes(*b)) == *n)
            }) {
                return Err(Error::Integrity);
            }
            let row = client.query_one("SELECT a.sequence,a.floor,s.changed,s.completed_at,s.expires_at FROM td_mta.accounts a JOIN td_mta.submissions s ON s.account=a.id WHERE a.id=$1 AND s.id=$2 FOR UPDATE OF s", &[&account,&submission]).await.map_err(|_| Error::Database)?;
            let old = number(row.try_get(0).map_err(|_| Error::Integrity)?)?;
            let next = old.checked_add(1).ok_or(Error::Capacity)?;
            let completed: Option<i64> = row.try_get(3).map_err(|_| Error::Integrity)?;
            let expires: i64 = row.try_get(4).map_err(|_| Error::Integrity)?;
            if number(row.try_get(1).map_err(|_| Error::Integrity)?)? > old
                || number(row.try_get(2).map_err(|_| Error::Integrity)?)? > old
                || completed.is_some()
                || timestamp >= expires
            {
                return Err(Error::Integrity);
            }
            let count: i64 = client.query_one("SELECT count(*) FROM (SELECT 1 FROM td_mta.attempt_phase_receipts WHERE account=$1 LIMIT 2048) receipts", &[&account]).await.map_err(|_| Error::Database)?.try_get(0).map_err(|_| Error::Integrity)?;
            if count >= 2048 {
                return Err(Error::Capacity);
            }
            let rows = client.query("SELECT ordinal,phase,rcpt_reply,data_reply,reason,changed,last_attempt_at,uncertain,attempt_count,next_attempt_at FROM td_mta.recipients WHERE account=$1 AND submission_id=$2 AND attempt_id=$3 AND state=2 ORDER BY ordinal LIMIT 101 FOR UPDATE", &[&account,&submission,&attempt]).await.map_err(|_| Error::Database)?;
            if rows.is_empty() || rows.len() > 100 {
                return Err(Error::Integrity);
            }
            let mut selected = 0;
            let mut body_count = 0;
            for row in &rows {
                let ordinal: i64 = row.try_get(0).map_err(|_| Error::Integrity)?;
                let phase: i64 = row.try_get(1).map_err(|_| Error::Integrity)?;
                let reply: Option<&str> = row.try_get(2).map_err(|_| Error::Integrity)?;
                let data: Option<&str> = row.try_get(3).map_err(|_| Error::Integrity)?;
                let reason: i64 = row.try_get(4).map_err(|_| Error::Integrity)?;
                let last: i64 = row.try_get(6).map_err(|_| Error::Integrity)?;
                let uncertain: i64 = row.try_get(7).map_err(|_| Error::Integrity)?;
                let attempts: i64 = row.try_get(8).map_err(|_| Error::Integrity)?;
                let retry: Option<i64> = row.try_get(9).map_err(|_| Error::Integrity)?;
                if !encoded
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|bytes| i64::from(u32::from_be_bytes(*bytes)) == ordinal)
                    || reason != 0
                    || number(row.try_get(5).map_err(|_| Error::Integrity)?)? > old
                    || last > timestamp
                    || !(0..=1).contains(&uncertain)
                    || attempts <= 0
                    || retry.is_some()
                {
                    return Err(Error::Integrity);
                }
                if phase == 2 {
                    body_count += 1;
                }
                if value.ordinals.binary_search(&ordinal).is_ok() {
                    selected += 1;
                    if phase != value.phase - 1
                        || (phase == 2
                            && (reply.is_none_or(|s| !positive_reply(s)) || data.is_some()))
                    {
                        return Err(Error::Conflict);
                    }
                } else if phase != 1 {
                    return Err(Error::Conflict);
                }
            }
            if selected != value.ordinals.len() || (value.phase == 3 && body_count != selected) {
                return Err(Error::Conflict);
            }
            let changed = next.to_be_bytes();
            let changed = changed.as_slice();
            let affected = if value.phase == 2 {
            client.execute("UPDATE td_mta.recipients r SET phase=2,rcpt_reply=u.reply,data_reply=NULL,changed=$5 FROM unnest($3::bigint[],$4::text[]) u(ordinal,reply) WHERE r.account=$1 AND r.submission_id=$2 AND r.ordinal=u.ordinal", &[&account,&submission,&value.ordinals,&value.replies,&changed]).await
        } else {
            client.execute("UPDATE td_mta.recipients SET phase=3,changed=$4 WHERE account=$1 AND submission_id=$2 AND ordinal=ANY($3)", &[&account,&submission,&value.ordinals,&changed]).await
        }.map_err(|_| Error::Database)?;
            if affected != value.ordinals.len() as u64 {
                return Err(Error::Integrity);
            }
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
            let recipients = i64::try_from(value.ordinals.len()).map_err(|_| Error::Capacity)?;
            client
                .execute(
                    "INSERT INTO td_mta.attempt_phase_receipts VALUES($1,$2,$3,$4,$5,$6,$7)",
                    &[
                        &account,
                        &attempt,
                        &value.phase,
                        &value.digest.as_slice(),
                        &recipients,
                        &old.to_be_bytes().as_slice(),
                        &changed,
                    ],
                )
                .await
                .map_err(|_| Error::Database)?;
            marker.store(true, Ordering::Relaxed);
            client
                .batch_execute("COMMIT")
                .await
                .map_err(|_| Error::Indeterminate)?;
            Ok(Outcome {
                attempt: value.attempt,
                recipients: u32::try_from(recipients).map_err(|_| Error::Integrity)?,
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
