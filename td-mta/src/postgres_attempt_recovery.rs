//! Fenced settlement of interrupted or abandoned recipient attempts.
use crate::{
    ids::{AttemptId, AttemptRecoveryId},
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Owner(owners::Error),
    Policy,
    Missing,
    CurrentOwner,
    StaleAttempt,
    Integrity,
    Capacity,
    Crypto,
    Clock,
    ExpiryHold,
    Conflict,
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
pub struct Recovery {
    claim: Claim,
    generation: u64,
    attempt: AttemptId,
    operation: AttemptRecoveryId,
    digest: [u8; 32],
    current: bool,
    positive_ordinals: Vec<i64>,
    positive_replies: Vec<String>,
}
impl Recovery {
    pub(crate) fn attempt(&self) -> AttemptId {
        self.attempt
    }
    pub fn operation(&self) -> AttemptRecoveryId {
        self.operation
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Outcome {
    pub attempt: AttemptId,
    pub recipients: u32,
    pub old_sequence: u64,
    pub new_sequence: u64,
}
pub fn prepare(claim: &Claim, generation: u64, attempt: AttemptId) -> Result<Recovery, Error> {
    prepare_mode(claim, generation, attempt, false)
}
/// Prepare settlement of the current owner's interrupted transport. The caller
/// must close/fence transport before publication; this never authorizes sends.
pub fn interrupted(claim: &Claim, generation: u64, attempt: AttemptId) -> Result<Recovery, Error> {
    prepare_mode(claim, generation, attempt, true)
}
/// Settle an interrupted current attempt with actual positive RCPT replies
/// not yet published as Body. The caller must fence transport before publish.
pub fn interrupted_with_rcpt(
    claim: &Claim,
    generation: u64,
    attempt: AttemptId,
    accepted: &[(u32, &str)],
) -> Result<Recovery, Error> {
    if accepted.is_empty() || accepted.len() > 100 {
        return Err(Error::Integrity);
    }
    let mut value = prepare_mode(claim, generation, attempt, true)?;
    value
        .positive_ordinals
        .try_reserve_exact(accepted.len())
        .map_err(|_| Error::Capacity)?;
    value
        .positive_replies
        .try_reserve_exact(accepted.len())
        .map_err(|_| Error::Capacity)?;
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    hash.update(b"td-mta interrupted positive RCPT evidence v1")
        .map_err(|_| Error::Crypto)?;
    hash.update(&value.digest).map_err(|_| Error::Crypto)?;
    hash.update(&(accepted.len() as u64).to_be_bytes())
        .map_err(|_| Error::Crypto)?;
    for (ordinal, reply) in accepted {
        let ordinal = i64::from(*ordinal);
        if ordinal >= 1000
            || value
                .positive_ordinals
                .last()
                .is_some_and(|last| *last >= ordinal)
            || crate::format::row::smtp_reply_text(reply).is_err()
            || crate::recipient_sweep::reply_class(Some(reply)) != Some(2)
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
        value.positive_ordinals.push(ordinal);
        value.positive_replies.push(retained);
    }
    value.digest = hash.finish().map_err(|_| Error::Crypto)?;
    Ok(value)
}
fn prepare_mode(
    claim: &Claim,
    generation: u64,
    attempt: AttemptId,
    current: bool,
) -> Result<Recovery, Error> {
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
        if current {
            b"td-mta attempt interruption v1".as_slice()
        } else {
            b"td-mta attempt recovery v1".as_slice()
        },
        binding.epoch().as_bytes().as_slice(),
        binding.account().as_bytes().as_slice(),
        claim.operation().as_bytes().as_slice(),
        claim.boot().as_bytes().as_slice(),
        &generation.to_be_bytes(),
        attempt.as_bytes().as_slice(),
        &operation,
    ] {
        hash.update(&(field.len() as u64).to_be_bytes())
            .map_err(|_| Error::Crypto)?;
        hash.update(field).map_err(|_| Error::Crypto)?;
    }
    Ok(Recovery {
        claim: claim.clone(),
        generation,
        attempt,
        operation: AttemptRecoveryId::from_bytes(operation),
        digest: hash.finish().map_err(|_| Error::Crypto)?,
        current,
        positive_ordinals: Vec::new(),
        positive_replies: Vec::new(),
    })
}
fn number(bytes: &[u8]) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    ))
}
async fn receipt(client: &Client, value: &Recovery) -> Result<Option<Outcome>, Error> {
    let rows=client.query("SELECT request_digest,attempt_id,recipient_count,old_sequence,new_sequence FROM td_mta.attempt_recovery_receipts WHERE account=$1 AND operation=$2", &[&value.claim.binding().account().as_bytes().as_slice(),&value.operation.as_bytes().as_slice()]).await.map_err(|_|Error::Database)?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let digest: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
    if digest != value.digest {
        return Err(Error::Conflict);
    }
    let attempt: &[u8] = row.try_get(1).map_err(|_| Error::Integrity)?;
    let count: i64 = row.try_get(2).map_err(|_| Error::Integrity)?;
    let old = number(row.try_get(3).map_err(|_| Error::Integrity)?)?;
    let new = number(row.try_get(4).map_err(|_| Error::Integrity)?)?;
    if attempt != value.attempt.as_bytes()
        || !(1..=100).contains(&count)
        || old.checked_add(1) != Some(new)
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
    value: &Recovery,
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
pub(crate) fn retry_delay(entropy: &mut impl Entropy, count: u32) -> Result<i64, Error> {
    let exponent = count.checked_sub(1).ok_or(Error::Integrity)?.min(4);
    let nominal = (300_000u64
        .checked_mul(1u64 << exponent)
        .ok_or(Error::Capacity)?)
    .min(3_600_000);
    let low = (nominal * 9 / 10).max(300_000);
    let high = (nominal * 11 / 10).min(3_600_000);
    let width = high - low + 1;
    let limit = u64::MAX - u64::MAX % width;
    for _ in 0..32 {
        let mut bytes = [0; 8];
        entropy.fill(&mut bytes).map_err(|_| Error::Crypto)?;
        let sample = u64::from_be_bytes(bytes);
        if sample < limit {
            return i64::try_from(low + sample % width).map_err(|_| Error::Capacity);
        }
    }
    Err(Error::Crypto)
}
/// Settle an older abandoned generation or the interrupted current attempt.
/// Near-expiry work waits for clock-confirmed expiry. An unknown COMMIT must
/// settle historically, without publishing an alternative or resuming transport.
pub fn publish(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Recovery,
) -> Result<Outcome, Error> {
    if let Some(outcome) = inspect(request, trust, deadline, value)? {
        return Ok(outcome);
    }
    let value = value.clone();
    let mut entropy = SystemEntropy::try_new().map_err(|_| Error::Crypto)?;
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
            let account = binding.account();
            let account = account.as_bytes().as_slice();
            let attempt = value.attempt.as_bytes().as_slice();
            let rows=client.query("SELECT submission_id,generation,ordinals,owner_boot,owner_operation FROM td_mta.submission_attempts WHERE account=$1 AND attempt_id=$2 AND route_name=$3 AND destination=$4 AND active FOR UPDATE", &[&account,&attempt,&binding.route().name(),&binding.route().destination().as_slice()]).await.map_err(|_|Error::Database)?;
            let row = rows.first().ok_or(Error::Missing)?;
            let submission: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
            let prior = number(row.try_get(1).map_err(|_| Error::Integrity)?)?;
            if value.current {
                let boot: &[u8] = row.try_get(3).map_err(|_| Error::Integrity)?;
                let operation: &[u8] = row.try_get(4).map_err(|_| Error::Integrity)?;
                if prior != value.generation
                    || boot != value.claim.boot().as_bytes()
                    || operation != value.claim.operation().as_bytes()
                {
                    return Err(Error::StaleAttempt);
                }
            } else if prior >= value.generation {
                return Err(Error::CurrentOwner);
            }
            let encoded: &[u8] = row.try_get(2).map_err(|_| Error::Integrity)?;
            if submission.len() != 16
                || prior == 0
                || encoded.is_empty()
                || encoded.len() > 400
                || !encoded.len().is_multiple_of(4)
            {
                return Err(Error::Integrity);
            }
            let mut ordinals = Vec::new();
            ordinals
                .try_reserve_exact(encoded.len() / 4)
                .map_err(|_| Error::Capacity)?;
            for chunk in encoded.as_chunks::<4>().0 {
                let n = u32::from_be_bytes(*chunk);
                if n >= 1000 || ordinals.last().is_some_and(|last| *last >= n) {
                    return Err(Error::Integrity);
                }
                ordinals.push(n);
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
            let rows=client.query("SELECT expires_at,completed_at,changed FROM td_mta.submissions WHERE account=$1 AND id=$2 FOR UPDATE", &[&account,&submission]).await.map_err(|_|Error::Database)?;
            let row = rows.first().ok_or(Error::Integrity)?;
            let expires: i64 = row.try_get(0).map_err(|_| Error::Integrity)?;
            let completed: Option<i64> = row.try_get(1).map_err(|_| Error::Integrity)?;
            if completed.is_some() || number(row.try_get(2).map_err(|_| Error::Integrity)?)? > old {
                return Err(Error::Integrity);
            }
            let rows=client.query("SELECT ordinal,uncertain,attempt_count,last_attempt_at,phase,rcpt_reply,data_reply,reason,changed FROM td_mta.recipients WHERE account=$1 AND submission_id=$2 AND attempt_id=$3 AND state=2 ORDER BY ordinal LIMIT 101 FOR UPDATE", &[&account,&submission,&attempt]).await.map_err(|_|Error::Database)?;
            if rows.is_empty() || rows.len() > 100 {
                return Err(Error::Integrity);
            }
            let mut possible = Vec::new();
            let mut selected = Vec::new();
            let mut states = Vec::new();
            let mut uncertainties = Vec::new();
            let mut retries = Vec::new();
            let mut reasons = Vec::new();
            for buffer in [
                &mut possible,
                &mut selected,
                &mut states,
                &mut uncertainties,
                &mut retries,
                &mut reasons,
            ] {
                buffer
                    .try_reserve_exact(rows.len())
                    .map_err(|_| Error::Capacity)?;
            }
            let mut matched_positive = 0usize;
            for row in &rows {
                let ordinal: i64 = row.try_get(0).map_err(|_| Error::Integrity)?;
                let uncertain: i64 = row.try_get(1).map_err(|_| Error::Integrity)?;
                let count: i64 = row.try_get(2).map_err(|_| Error::Integrity)?;
                let last: i64 = row.try_get(3).map_err(|_| Error::Integrity)?;
                let phase: i64 = row.try_get(4).map_err(|_| Error::Integrity)?;
                let rcpt: Option<&str> = row.try_get(5).map_err(|_| Error::Integrity)?;
                let data: Option<&str> = row.try_get(6).map_err(|_| Error::Integrity)?;
                let reason: i64 = row.try_get(7).map_err(|_| Error::Integrity)?;
                let ordinal32 = u32::try_from(ordinal).map_err(|_| Error::Integrity)?;
                if ordinals.binary_search(&ordinal32).is_err()
                    || !(0..=1).contains(&uncertain)
                    || !(1..=3).contains(&phase)
                    || reason != 0
                    || number(row.try_get(8).map_err(|_| Error::Integrity)?)? > old
                {
                    return Err(Error::Integrity);
                }
                if phase != 1
                    && (crate::recipient_sweep::reply_class(rcpt) != Some(2) || data.is_some())
                {
                    return Err(Error::Integrity);
                }
                if value.positive_ordinals.binary_search(&ordinal).is_ok() {
                    if !value.current || phase != 1 {
                        return Err(Error::Integrity);
                    }
                    matched_positive += 1;
                }
                if timestamp < last {
                    return Err(Error::Clock);
                }
                let delay = retry_delay(
                    &mut entropy,
                    u32::try_from(count).map_err(|_| Error::Integrity)?,
                )?;
                let retry = timestamp.checked_add(delay).ok_or(Error::Clock)?;
                if retry >= expires {
                    return Err(Error::ExpiryHold);
                }
                let uncertain = i64::from(uncertain == 1 || phase == 3);
                let state = if uncertain == 1 { 7i64 } else { 3i64 };
                let reason = if phase == 3 { 9i64 } else { 3i64 };
                if phase == 3 {
                    possible.push(ordinal);
                }
                selected.push(ordinal);
                states.push(state);
                uncertainties.push(uncertain);
                retries.push(retry);
                reasons.push(reason);
            }
            if matched_positive != value.positive_ordinals.len() {
                return Err(Error::Integrity);
            }
            crate::postgres_final_capacity::consume(client, account, attempt, &possible)
                .await
                .map_err(capacity)?;
            crate::postgres_final_capacity::admit_receipt(
                client,
                account,
                crate::postgres_final_capacity::Receipt::Recovery,
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
            if !value.positive_ordinals.is_empty()
                && client.execute("UPDATE td_mta.recipients r SET rcpt_reply=p.reply,data_reply=NULL FROM unnest($4::bigint[],$5::text[]) p(ordinal,reply) WHERE r.account=$1 AND r.submission_id=$2 AND r.attempt_id=$3 AND r.state=2 AND r.phase=1 AND r.ordinal=p.ordinal", &[&account,&submission,&attempt,&value.positive_ordinals,&value.positive_replies]).await.map_err(|_|Error::Database)? != value.positive_ordinals.len() as u64
            {
                return Err(Error::Integrity);
            }
            if client.execute("UPDATE td_mta.recipients r SET state=u.state,uncertain=u.uncertain,phase=4,next_attempt_at=u.retry,reason=u.reason,diagnostic='',changed=$8 FROM unnest($3::bigint[],$4::bigint[],$5::bigint[],$6::bigint[],$7::bigint[]) u(ordinal,state,uncertain,retry,reason) WHERE r.account=$1 AND r.submission_id=$2 AND r.ordinal=u.ordinal", &[&account,&submission,&selected,&states,&uncertainties,&retries,&reasons,&changed]).await.map_err(|_|Error::Database)?!=rows.len() as u64 {return Err(Error::Integrity)}
            if client.execute("UPDATE td_mta.submission_attempts SET active=false WHERE account=$1 AND attempt_id=$2 AND active", &[&account,&attempt]).await.map_err(|_|Error::Database)?!=1 {return Err(Error::Integrity)}
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
            let count = i64::try_from(rows.len()).map_err(|_| Error::Capacity)?;
            client
                .execute(
                    "INSERT INTO td_mta.attempt_recovery_receipts VALUES($1,$2,$3,$4,$5,$6,$7)",
                    &[
                        &account,
                        &value.operation.as_bytes().as_slice(),
                        &value.digest.as_slice(),
                        &attempt,
                        &count,
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
                recipients: u32::try_from(count).map_err(|_| Error::Integrity)?,
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    struct Fixed(u64, usize);
    impl Entropy for Fixed {
        fn fill(&mut self, output: &mut [u8]) -> Result<(), td_crypto::Error> {
            output.copy_from_slice(&self.0.to_be_bytes());
            self.1 += 1;
            Ok(())
        }
    }
    #[test]
    fn retry_jitter_clips_before_sampling_and_bounds_rejection() {
        for (count, low, high) in [
            (1, 300_000, 330_000),
            (2, 540_000, 660_000),
            (5, 3_240_000, 3_600_000),
            (u32::MAX, 3_240_000, 3_600_000),
        ] {
            assert_eq!(retry_delay(&mut Fixed(0, 0), count).unwrap(), low);
            assert_eq!(
                retry_delay(&mut Fixed((high - low) as u64, 0), count).unwrap(),
                high
            );
        }
        assert_eq!(retry_delay(&mut Fixed(0, 0), 0), Err(Error::Integrity));
        let mut rejected = Fixed(u64::MAX, 0);
        assert_eq!(retry_delay(&mut rejected, 1), Err(Error::Crypto));
        assert_eq!(rejected.1, 32);
    }
}
