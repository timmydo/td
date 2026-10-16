//! Durable DATA phases and final replies under the current attempt fence.
use crate::{
    ids::AttemptId,
    ports::Clock,
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
use td_crypto::{Crypto, Digest, Provider, SystemEntropy, TrustStore};
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
    ExpiryHold,
    Clock(queue_clock::Error),
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
pub struct Change {
    inner: Arc<Fields>,
}
struct Fields {
    claim: Claim,
    generation: u64,
    attempt: AttemptId,
    phase: i64,
    ordinals: Vec<i64>,
    replies: Vec<String>,
    rcpt_replies: Vec<String>,
    digest: [u8; 32],
}
impl Change {
    pub(crate) fn is_final_for(&self, attempt: AttemptId) -> bool {
        self.inner.attempt == attempt && matches!(self.inner.phase, 4 | 5)
    }
    pub(crate) fn attempt(&self) -> AttemptId {
        self.inner.attempt
    }
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
/// Supply the entire previously committed Body subset. Publication reserves
/// durable logical result capacity; the caller still needs local capacity and
/// current ownership before emitting a terminator.
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
/// Retain a genuine final positive DATA reply for the entire transmitted
/// subset. This records handoff, not final mailbox delivery or deduplication.
pub fn accepted(
    claim: &Claim,
    generation: u64,
    attempt: AttemptId,
    ordinals: &[u32],
    reply: &str,
) -> Result<Change, Error> {
    prepare(
        claim,
        generation,
        attempt,
        4,
        ordinals.iter().map(|n| (*n, reply)),
    )
}

/// Retain a definitive final negative DATA reply for the entire transmitted
/// subset. A temporary refusal near expiry waits for clock-confirmed expiry.
pub fn refused(
    claim: &Claim,
    generation: u64,
    attempt: AttemptId,
    ordinals: &[u32],
    reply: &str,
) -> Result<Change, Error> {
    prepare(
        claim,
        generation,
        attempt,
        5,
        ordinals.iter().map(|n| (*n, reply)),
    )
}

/// Preallocated storage for one actual final DATA reply. This conveys no
/// permission to send a terminator or publish a phase.
pub struct FinalReply {
    value: Change,
    positive: td_crypto::Sha256,
    negative: td_crypto::Sha256,
}
/// Allocate bounded final-result evidence before AcceptancePossible. Native
/// database client and SQL row allocations remain separately bounded.
pub fn reserve_final_reply(
    claim: &Claim,
    generation: u64,
    attempt: AttemptId,
    ordinals: &[u32],
) -> Result<FinalReply, Error> {
    let mut value = prepare(
        claim,
        generation,
        attempt,
        3,
        ordinals.iter().map(|n| (*n, "")),
    )?;
    let inner = Arc::get_mut(&mut value.inner).ok_or(Error::Integrity)?;
    for reply in &mut inner.replies {
        reply
            .try_reserve_exact(crate::format::row::MAX_SMTP_REPLY)
            .map_err(|_| Error::Capacity)?;
    }
    Ok(FinalReply {
        value,
        positive: hash_prefix(claim, generation, attempt, 4)?,
        negative: hash_prefix(claim, generation, attempt, 5)?,
    })
}
impl FinalReply {
    /// Retain only a genuine normalized positive/negative final DATA reply.
    /// The existing digest format is preserved for replay and settlement.
    pub fn finish(self, reply: &str) -> Result<Change, Error> {
        let phase = if valid_reply(reply, false) {
            4
        } else if valid_reply(reply, true) {
            5
        } else {
            return Err(Error::Integrity);
        };
        let Self {
            mut value,
            positive,
            negative,
        } = self;
        let hash = if phase == 4 { positive } else { negative };
        let inner = Arc::get_mut(&mut value.inner).ok_or(Error::Integrity)?;
        inner.phase = phase;
        inner.digest = fill_final_replies(&inner.ordinals, &mut inner.replies, reply, hash)?;
        Ok(value)
    }
}

fn fill_final_replies(
    ordinals: &[i64],
    replies: &mut [String],
    reply: &str,
    mut hash: td_crypto::Sha256,
) -> Result<[u8; 32], Error> {
    if ordinals.len() != replies.len() {
        return Err(Error::Integrity);
    }
    for (ordinal, retained) in ordinals.iter().zip(replies) {
        if !retained.is_empty() || retained.capacity() < reply.len() {
            return Err(Error::Capacity);
        }
        retained.push_str(reply);
        hash.update(&ordinal.to_be_bytes())
            .map_err(|_| Error::Crypto)?;
        hash.update(&(reply.len() as u64).to_be_bytes())
            .map_err(|_| Error::Crypto)?;
        hash.update(reply.as_bytes()).map_err(|_| Error::Crypto)?;
    }
    hash.finish().map_err(|_| Error::Crypto)
}

/// Retain all negative RCPT replies collected for one attempt, as one ordered
/// subset. Other Prepared recipients may subsequently enter Body.
pub fn refused_rcpt(
    claim: &Claim,
    generation: u64,
    attempt: AttemptId,
    recipients: &[(u32, &str)],
) -> Result<Change, Error> {
    prepare(claim, generation, attempt, 6, recipients.iter().copied())
}

/// Retain a negative MAIL reply for the entire still-Prepared attempt.
/// This clears earlier RCPT history and stores the actual message refusal.
pub fn refused_mail(
    claim: &Claim,
    generation: u64,
    attempt: AttemptId,
    ordinals: &[u32],
    reply: &str,
) -> Result<Change, Error> {
    prepare(
        claim,
        generation,
        attempt,
        7,
        ordinals.iter().map(|n| (*n, reply)),
    )
}

/// Retain a negative DATA-command reply, before any Body phase, together with
/// the actual positive RCPT replies for precisely its accepted subset.
pub fn refused_data_command(
    claim: &Claim,
    generation: u64,
    attempt: AttemptId,
    accepted: &[(u32, &str)],
    reply: &str,
) -> Result<Change, Error> {
    let mut value = prepare(
        claim,
        generation,
        attempt,
        8,
        accepted.iter().map(|(n, _)| (*n, reply)),
    )?;
    let inner = Arc::get_mut(&mut value.inner).ok_or(Error::Integrity)?;
    inner
        .rcpt_replies
        .try_reserve_exact(accepted.len())
        .map_err(|_| Error::Capacity)?;
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    hash.update(b"td-mta DATA-command RCPT evidence v1")
        .map_err(|_| Error::Crypto)?;
    hash.update(&inner.digest).map_err(|_| Error::Crypto)?;
    for (_, rcpt) in accepted {
        if !valid_reply(rcpt, false) {
            return Err(Error::Integrity);
        }
        let mut retained = String::new();
        retained
            .try_reserve_exact(rcpt.len())
            .map_err(|_| Error::Capacity)?;
        retained.push_str(rcpt);
        hash.update(&(rcpt.len() as u64).to_be_bytes())
            .map_err(|_| Error::Crypto)?;
        hash.update(rcpt.as_bytes()).map_err(|_| Error::Crypto)?;
        inner.rcpt_replies.push(retained);
    }
    inner.digest = hash.finish().map_err(|_| Error::Crypto)?;
    Ok(value)
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
    let mut hash = hash_prefix(claim, generation, attempt, phase)?;
    for (ordinal, reply) in recipients {
        let ordinal = i64::from(ordinal);
        if ordinals.len() == 100
            || ordinal >= 1000
            || ordinals.last().is_some_and(|last| *last >= ordinal)
            || (phase != 3 && !valid_reply(reply, phase >= 5))
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
        inner: Arc::new(Fields {
            claim: claim.clone(),
            generation,
            attempt,
            phase,
            ordinals,
            replies,
            rcpt_replies: Vec::new(),
            digest: hash.finish().map_err(|_| Error::Crypto)?,
        }),
    })
}
fn hash_prefix(
    claim: &Claim,
    generation: u64,
    attempt: AttemptId,
    phase: i64,
) -> Result<td_crypto::Sha256, Error> {
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
    Ok(hash)
}
fn valid_reply(reply: &str, negative: bool) -> bool {
    crate::format::row::smtp_reply_text(reply).is_ok()
        && if negative {
            matches!(
                crate::recipient_sweep::reply_class(Some(reply)),
                Some(4 | 5)
            )
        } else {
            crate::recipient_sweep::reply_class(Some(reply)) == Some(2)
        }
}
fn number(bytes: &[u8]) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    ))
}
async fn receipt(client: &Client, value: &Fields) -> Result<Option<Outcome>, Error> {
    let query = if value.phase >= 7 {
        "SELECT request_digest,recipient_count,old_sequence,new_sequence FROM td_mta.attempt_command_receipts WHERE account=$1 AND attempt_id=$2 AND phase=$3"
    } else if value.phase == 6 {
        "SELECT request_digest,recipient_count,old_sequence,new_sequence FROM td_mta.attempt_rcpt_receipts WHERE account=$1 AND attempt_id=$2 AND phase=$3"
    } else if value.phase == 5 {
        "SELECT request_digest,recipient_count,old_sequence,new_sequence FROM td_mta.attempt_refusal_receipts WHERE account=$1 AND attempt_id=$2 AND phase=$3"
    } else if value.phase == 4 {
        "SELECT request_digest,recipient_count,old_sequence,new_sequence FROM td_mta.attempt_acceptance_receipts WHERE account=$1 AND attempt_id=$2 AND phase=$3"
    } else {
        "SELECT request_digest,recipient_count,old_sequence,new_sequence FROM td_mta.attempt_phase_receipts WHERE account=$1 AND attempt_id=$2 AND phase=$3"
    };
    let rows = client
        .query(
            query,
            &[
                &value.claim.binding().account().as_bytes().as_slice(),
                &value.attempt.as_bytes().as_slice(),
                &value.phase,
            ],
        )
        .await
        .map_err(|_| Error::Database)?;
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
    let value = value.inner.clone();
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
/// Publishes the retained phase or final outcome. Unknown COMMIT requires the
/// caller to close transport; receipt settlement is historical evidence only.
pub fn publish(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Change,
) -> Result<Outcome, Error> {
    publish_inner(request, trust, deadline, value, false, None)
}
/// Publish only a new AcceptancePossible transition. An existing receipt,
/// including an identical one, refuses; inspect/ordinary publish remain the
/// historical settlement path. Novelty alone does not authorize SMTP output.
pub fn publish_new_possible(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Change,
) -> Result<Outcome, Error> {
    if value.inner.phase != 3 {
        return Err(Error::Integrity);
    }
    publish_inner(request, trust, deadline, value, true, None)
}
/// New publication with persistent queue-clock observation at admission and
/// before COMMIT. Neither its receipt nor its clock samples authorize SMTP.
pub fn publish_new_possible_observed(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Change,
    clock: Arc<dyn Clock>,
    guard: Arc<Mutex<Guard>>,
) -> Result<Outcome, Error> {
    if value.inner.phase != 3 {
        return Err(Error::Integrity);
    }
    publish_inner(
        request,
        trust,
        deadline,
        value,
        true,
        Some(Observation { clock, guard }),
    )
}
struct Observation {
    clock: Arc<dyn Clock>,
    guard: Arc<Mutex<Guard>>,
}
impl Observation {
    async fn check(&self, client: &Client, value: &Fields) -> Result<i64, Error> {
        owners::check_locked_observed(client, &value.claim, value.generation, |utc_ms| {
            queue_clock::admit_database(self.clock.as_ref(), &self.guard, utc_ms, false)
                .map_err(Error::Clock)
        })
        .await
    }
}
impl From<owners::Error> for Error {
    fn from(error: owners::Error) -> Self {
        Self::Owner(error)
    }
}
fn publish_inner(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Change,
    new_possible: bool,
    observation: Option<Observation>,
) -> Result<Outcome, Error> {
    if let Some(outcome) = inspect(request, trust, deadline, value)? {
        return if new_possible {
            Err(Error::Conflict)
        } else {
            Ok(outcome)
        };
    }
    let value = value.inner.clone();
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_batch_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let policy = owners::begin(client, &value.claim, true, true)
                .await
                .map_err(Error::Owner)?;
            if let Some(outcome) = receipt(client, &value).await? {
                return if new_possible {
                    Err(Error::Conflict)
                } else {
                    Ok(outcome)
                };
            }
            if !policy {
                return Err(Error::Policy);
            }
            let timestamp = if let Some(observation) = &observation {
                observation.check(client, &value).await?
            } else {
                owners::check_locked(client, &value.claim, value.generation)
                    .await
                    .map_err(Error::Owner)?
            };
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
            if value.phase == 7 && value.ordinals.len() != encoded.len() / 4 {
                return Err(Error::Conflict);
            }
            if value.phase == 2 || value.phase == 8 {
                // Finished recipients no longer appear in the InFlight scan.
                let query = if value.phase == 2 {
                    "SELECT 1 FROM td_mta.attempt_command_receipts WHERE account=$1 AND attempt_id=$2 AND phase=8"
                } else {
                    "SELECT 1 FROM td_mta.attempt_phase_receipts WHERE account=$1 AND attempt_id=$2 AND phase=2"
                };
                if !client
                    .query(query, &[&account, &attempt])
                    .await
                    .map_err(|_| Error::Database)?
                    .is_empty()
                {
                    return Err(Error::Conflict);
                }
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
            let completed: Option<i64> = row.try_get(3).map_err(|_| Error::Integrity)?;
            let expires: i64 = row.try_get(4).map_err(|_| Error::Integrity)?;
            if number(row.try_get(1).map_err(|_| Error::Integrity)?)? > old
                || number(row.try_get(2).map_err(|_| Error::Integrity)?)? > old
                || completed.is_some()
                || (value.phase < 4 && timestamp >= expires)
            {
                return Err(Error::Integrity);
            }
            if value.phase != 4 && value.phase != 5 {
                let (count_query, ceiling) = if value.phase >= 7 {
                    ("SELECT count(*) FROM (SELECT 1 FROM td_mta.attempt_command_receipts WHERE account=$1 LIMIT 1024) receipts", 1024)
                } else if value.phase == 6 {
                    ("SELECT count(*) FROM (SELECT 1 FROM td_mta.attempt_rcpt_receipts WHERE account=$1 LIMIT 1024) receipts", 1024)
                } else {
                    ("SELECT count(*) FROM (SELECT 1 FROM td_mta.attempt_phase_receipts WHERE account=$1 LIMIT 2048) receipts", 2048)
                };
                let count: i64 = client
                    .query_one(count_query, &[&account])
                    .await
                    .map_err(|_| Error::Database)?
                    .try_get(0)
                    .map_err(|_| Error::Integrity)?;
                if count >= ceiling {
                    return Err(Error::Capacity);
                }
            }
            let rows = client.query("SELECT ordinal,phase,rcpt_reply,data_reply,reason,changed,last_attempt_at,uncertain,attempt_count,next_attempt_at FROM td_mta.recipients WHERE account=$1 AND submission_id=$2 AND attempt_id=$3 AND state=2 ORDER BY ordinal LIMIT 101 FOR UPDATE", &[&account,&submission,&attempt]).await.map_err(|_| Error::Database)?;
            if rows.is_empty() || rows.len() > 100 {
                return Err(Error::Integrity);
            }
            let prior_phase = if value.phase >= 6 {
                1
            } else if value.phase >= 4 {
                3
            } else {
                value.phase - 1
            };
            let mut entropy = if value.phase >= 5 {
                Some(SystemEntropy::try_new().map_err(|_| Error::Crypto)?)
            } else {
                None
            };
            let mut states = Vec::new();
            let mut reasons = Vec::new();
            reasons
                .try_reserve_exact(value.ordinals.len())
                .map_err(|_| Error::Capacity)?;
            let mut retries: Vec<Option<i64>> = Vec::new();
            states
                .try_reserve_exact(value.ordinals.len())
                .map_err(|_| Error::Capacity)?;
            retries
                .try_reserve_exact(value.ordinals.len())
                .map_err(|_| Error::Capacity)?;
            let mut selected = 0;
            let mut prior_count = 0;
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
                if phase == prior_phase {
                    prior_count += 1;
                }
                if let Ok(index) = value.ordinals.binary_search(&ordinal) {
                    selected += 1;
                    if phase != prior_phase
                        || (phase >= 2
                            && (reply.is_none_or(|s| !valid_reply(s, false)) || data.is_some()))
                    {
                        return Err(Error::Conflict);
                    }
                    if value.phase >= 5 {
                        let reply = value.replies.get(index).ok_or(Error::Integrity)?;
                        let permanent = crate::recipient_sweep::reply_class(Some(reply)) == Some(5);
                        let retry = if permanent {
                            None
                        } else {
                            let delay = crate::postgres_attempt_recovery::retry_delay(
                                entropy.as_mut().ok_or(Error::Integrity)?,
                                u32::try_from(attempts).map_err(|_| Error::Integrity)?,
                            )
                            .map_err(|error| match error {
                                crate::postgres_attempt_recovery::Error::Crypto => Error::Crypto,
                                crate::postgres_attempt_recovery::Error::Capacity => {
                                    Error::Capacity
                                }
                                _ => Error::Integrity,
                            })?;
                            let retry = timestamp.checked_add(delay).ok_or(Error::Capacity)?;
                            if retry >= expires {
                                return Err(Error::ExpiryHold);
                            }
                            Some(retry)
                        };
                        states.push(if uncertain == 1 {
                            7i64
                        } else if permanent {
                            5i64
                        } else {
                            3i64
                        });
                        retries.push(retry);
                        reasons.push(if permanent { 2i64 } else { 1i64 });
                    }
                } else if phase != 1 {
                    return Err(Error::Conflict);
                }
            }
            if selected != value.ordinals.len()
                || (((3..=5).contains(&value.phase) || value.phase == 7) && prior_count != selected)
            {
                return Err(Error::Conflict);
            }
            if value.phase == 3 {
                crate::postgres_final_capacity::reserve(client, account, attempt, &value.ordinals)
                    .await
                    .map_err(capacity)?;
            } else if value.phase == 4 || value.phase == 5 {
                crate::postgres_final_capacity::consume(client, account, attempt, &value.ordinals)
                    .await
                    .map_err(capacity)?;
                let kind = if value.phase == 4 {
                    crate::postgres_final_capacity::Receipt::Accepted
                } else {
                    crate::postgres_final_capacity::Receipt::Refused
                };
                crate::postgres_final_capacity::admit_receipt(client, account, kind)
                    .await
                    .map_err(capacity)?;
            }
            let next = crate::postgres_sequence::next(client, account, old)
                .await
                .map_err(|error| match error {
                    crate::postgres_sequence::Error::Capacity => Error::Capacity,
                    crate::postgres_sequence::Error::Integrity => Error::Integrity,
                    crate::postgres_sequence::Error::Database => Error::Database,
                })?;
            let changed = next.to_be_bytes();
            let changed = changed.as_slice();
            let affected = if value.phase == 2 {
            client.execute("UPDATE td_mta.recipients r SET phase=2,rcpt_reply=u.reply,data_reply=NULL,changed=$5 FROM unnest($3::bigint[],$4::text[]) u(ordinal,reply) WHERE r.account=$1 AND r.submission_id=$2 AND r.ordinal=u.ordinal", &[&account,&submission,&value.ordinals,&value.replies,&changed]).await
        } else if value.phase == 8 {
            client.execute("UPDATE td_mta.recipients r SET state=u.state,phase=4,rcpt_reply=u.rcpt,data_reply=u.reply,next_attempt_at=u.retry,reason=u.reason,diagnostic='',changed=$9 FROM unnest($3::bigint[],$4::bigint[],$5::text[],$6::bigint[],$7::bigint[],$8::text[]) u(ordinal,state,reply,retry,reason,rcpt) WHERE r.account=$1 AND r.submission_id=$2 AND r.ordinal=u.ordinal", &[&account,&submission,&value.ordinals,&states,&value.replies,&retries,&reasons,&value.rcpt_replies,&changed]).await
        } else if value.phase >= 5 {
            let query = if value.phase == 7 {
                "UPDATE td_mta.recipients r SET state=u.state,phase=4,rcpt_reply=NULL,data_reply=u.reply,next_attempt_at=u.retry,reason=u.reason,diagnostic='',changed=$8 FROM unnest($3::bigint[],$4::bigint[],$5::text[],$6::bigint[],$7::bigint[]) u(ordinal,state,reply,retry,reason) WHERE r.account=$1 AND r.submission_id=$2 AND r.ordinal=u.ordinal"
            } else if value.phase == 6 {
                "UPDATE td_mta.recipients r SET state=u.state,phase=4,rcpt_reply=u.reply,data_reply=NULL,next_attempt_at=u.retry,reason=u.reason,diagnostic='',changed=$8 FROM unnest($3::bigint[],$4::bigint[],$5::text[],$6::bigint[],$7::bigint[]) u(ordinal,state,reply,retry,reason) WHERE r.account=$1 AND r.submission_id=$2 AND r.ordinal=u.ordinal"
            } else {
                "UPDATE td_mta.recipients r SET state=u.state,phase=4,data_reply=u.reply,next_attempt_at=u.retry,reason=u.reason,diagnostic='',changed=$8 FROM unnest($3::bigint[],$4::bigint[],$5::text[],$6::bigint[],$7::bigint[]) u(ordinal,state,reply,retry,reason) WHERE r.account=$1 AND r.submission_id=$2 AND r.ordinal=u.ordinal"
            };
            client.execute(query, &[&account,&submission,&value.ordinals,&states,&value.replies,&retries,&reasons,&changed]).await
        } else if value.phase == 4 {
            client.execute("UPDATE td_mta.recipients r SET state=4,phase=4,data_reply=u.reply,changed=$5 FROM unnest($3::bigint[],$4::text[]) u(ordinal,reply) WHERE r.account=$1 AND r.submission_id=$2 AND r.ordinal=u.ordinal", &[&account,&submission,&value.ordinals,&value.replies,&changed]).await
        } else {
            client.execute("UPDATE td_mta.recipients SET phase=3,changed=$4 WHERE account=$1 AND submission_id=$2 AND ordinal=ANY($3)", &[&account,&submission,&value.ordinals,&changed]).await
        }.map_err(|_| Error::Database)?;
            if affected != value.ordinals.len() as u64 {
                return Err(Error::Integrity);
            }
            if value.phase >= 4 {
                let totals = client.query_one("SELECT count(*),min(ordinal),max(ordinal),count(*) FILTER (WHERE state IN (1,2,3) OR (state=7 AND next_attempt_at IS NOT NULL)),count(*) FILTER (WHERE state IN (5,7)) FROM td_mta.recipients WHERE account=$1 AND submission_id=$2", &[&account,&submission]).await.map_err(|_| Error::Database)?;
                let count: i64 = totals.try_get(0).map_err(|_| Error::Integrity)?;
                let first: Option<i64> = totals.try_get(1).map_err(|_| Error::Integrity)?;
                let last: Option<i64> = totals.try_get(2).map_err(|_| Error::Integrity)?;
                let pending: i64 = totals.try_get(3).map_err(|_| Error::Integrity)?;
                let failed: i64 = totals.try_get(4).map_err(|_| Error::Integrity)?;
                let expected: i64 = client
                    .query_one(
                        "SELECT recipient_count FROM td_mta.submissions WHERE account=$1 AND id=$2",
                        &[&account, &submission],
                    )
                    .await
                    .map_err(|_| Error::Database)?
                    .try_get(0)
                    .map_err(|_| Error::Integrity)?;
                if !(1..=1000).contains(&count)
                    || count != expected
                    || first != Some(0)
                    || last != count.checked_sub(1)
                {
                    return Err(Error::Integrity);
                }
                if pending == 0 {
                    client.execute("UPDATE td_mta.submissions SET completed_at=$3,notification=CASE WHEN notification=0 AND $4 THEN 1 ELSE notification END WHERE account=$1 AND id=$2 AND completed_at IS NULL", &[&account,&submission,&timestamp,&(failed != 0)]).await.map_err(|_| Error::Database)?;
                }
                client.execute("UPDATE td_mta.submission_attempts SET active=false WHERE account=$1 AND attempt_id=$2 AND NOT EXISTS (SELECT 1 FROM td_mta.recipients WHERE account=$1 AND submission_id=$3 AND attempt_id=$2 AND state=2)", &[&account,&attempt,&submission]).await.map_err(|_| Error::Database)?;
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
            let insert = if value.phase >= 7 {
                "INSERT INTO td_mta.attempt_command_receipts VALUES($1,$2,$3,$4,$5,$6,$7)"
            } else if value.phase == 6 {
                "INSERT INTO td_mta.attempt_rcpt_receipts VALUES($1,$2,$3,$4,$5,$6,$7)"
            } else if value.phase == 5 {
                "INSERT INTO td_mta.attempt_refusal_receipts VALUES($1,$2,$3,$4,$5,$6,$7)"
            } else if value.phase == 4 {
                "INSERT INTO td_mta.attempt_acceptance_receipts VALUES($1,$2,$3,$4,$5,$6,$7)"
            } else {
                "INSERT INTO td_mta.attempt_phase_receipts VALUES($1,$2,$3,$4,$5,$6,$7)"
            };
            client
                .execute(
                    insert,
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
            if let Some(observation) = &observation {
                let final_time = observation.check(client, &value).await?;
                if final_time < timestamp || final_time >= expires {
                    return Err(Error::ExpiryHold);
                }
            }
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

#[cfg(test)]
mod reply_text_tests {
    #[test]
    fn retained_reply_validation_preserves_only_horizontal_tabs() {
        assert!(super::valid_reply("250 \taccepted", false));
        assert!(super::valid_reply("550 \trefused", true));
        assert!(!super::valid_reply("250 \taccepted", true));
        for control in ['\0', '\r', '\n', '\u{7f}', '\u{85}'] {
            assert!(!super::valid_reply(
                &format!("250 {control}accepted"),
                false
            ));
        }
    }
}

#[cfg(test)]
mod final_reply_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn final_reply_filling_preserves_allocations_and_refuses_missing_capacity() {
        let reply = format!("250 {}", "x".repeat(4092));
        let mut retained: Vec<_> = (0..2).map(|_| String::with_capacity(4096)).collect();
        let before: Vec<_> = retained
            .iter()
            .map(|s| (s.as_ptr(), s.capacity()))
            .collect();
        let digest =
            fill_final_replies(&[0, 999], &mut retained, &reply, Provider.sha256().unwrap())
                .unwrap();
        assert_eq!(
            digest,
            [
                0x4d, 0xcc, 0xc3, 0xa8, 0x5f, 0x2a, 0xb6, 0x63, 0x9b, 0xa3, 0x8a, 0x34, 0x7b, 0x5d,
                0xbd, 0xb3, 0xda, 0x81, 0xb8, 0xab, 0xca, 0xc2, 0x9c, 0x86, 0x06, 0x7b, 0x83, 0x65,
                0x47, 0x14, 0x63, 0x8b
            ]
        );
        assert!(retained.iter().all(|s| s == &reply));
        assert_eq!(
            retained
                .iter()
                .map(|s| (s.as_ptr(), s.capacity()))
                .collect::<Vec<_>>(),
            before
        );
        assert_eq!(
            fill_final_replies(&[0, 999], &mut retained, &reply, Provider.sha256().unwrap()),
            Err(Error::Capacity)
        );
        let mut empty = [String::new()];
        assert_eq!(
            fill_final_replies(&[0], &mut empty, &reply, Provider.sha256().unwrap()),
            Err(Error::Capacity)
        );
        assert_eq!(
            fill_final_replies(&[0, 1], &mut empty, &reply, Provider.sha256().unwrap()),
            Err(Error::Integrity)
        );
    }
}
