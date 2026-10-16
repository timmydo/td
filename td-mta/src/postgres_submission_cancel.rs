//! Whole-submission cancellation, reserved before closing owned transport.
use crate::{
    device_credentials::Presented,
    ids::{DeviceId, StoreEpoch, SubmissionCancelId, SubmissionId},
    postgres_devices,
    postgres_probe::{self, Request},
    postgres_route_owners::{self as owners, Claim},
    postgres_runtime_profile::Binding,
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
    Credential(postgres_devices::Error),
    Unauthenticated,
    Policy,
    Missing,
    CannotUnsend,
    Fence,
    Deadline,
    Conflict,
    StateMismatch,
    Capacity,
    Integrity,
    Crypto,
    Database,
    Indeterminate,
}
#[derive(Clone)]
pub struct Cancellation {
    claim: Claim,
    generation: u64,
    submission: SubmissionId,
    expected: Option<u64>,
    authorization: Option<Authorization>,
    operation: SubmissionCancelId,
    digest: [u8; 32],
}
#[derive(Clone)]
struct Authorization {
    binding: Binding,
    device: DeviceId,
}
/// Passive request identity. Publication checks the current device credential
/// while holding the same account lock as revocation.
pub struct ClientRequest<'a> {
    pub binding: &'a Binding,
    pub device: DeviceId,
    pub expected: Option<(StoreEpoch, u64)>,
}
fn client_policy(claim: &Claim, binding: &Binding) -> Result<(), Error> {
    let owner = claim.binding();
    if !binding.roles().jmap()
        || binding.account() != owner.account()
        || binding.epoch() != owner.epoch()
        || binding.selection() != owner.selection()
    {
        return Err(Error::Policy);
    }
    Ok(())
}
/// Prepare a client cancellation for a relay owner of the same account and
/// selected configuration. No password or authentication result is retained.
pub fn prepare_for_device(
    claim: &Claim,
    generation: u64,
    submission: SubmissionId,
    input: ClientRequest<'_>,
) -> Result<Cancellation, Error> {
    client_policy(claim, input.binding)?;
    if input
        .expected
        .is_some_and(|(epoch, _)| epoch != input.binding.epoch())
    {
        return Err(Error::StateMismatch);
    }
    prepare_inner(
        claim,
        generation,
        submission,
        input.expected.map(|(_, sequence)| sequence),
        Some(Authorization {
            binding: input.binding.clone(),
            device: input.device,
        }),
    )
}
impl Cancellation {
    pub fn operation(&self) -> SubmissionCancelId {
        self.operation
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Outcome {
    pub submission: SubmissionId,
    pub recipients: u32,
    pub changed: u32,
    pub completed_at: i64,
    pub old_sequence: u64,
    pub new_sequence: u64,
}
pub fn prepare(
    claim: &Claim,
    generation: u64,
    submission: SubmissionId,
) -> Result<Cancellation, Error> {
    prepare_inner(claim, generation, submission, None, None)
}
/// Bind a caller's account state to the whole cancellation. Epoch mismatch
/// refuses preparation; publication checks sequence under the account lock.
pub fn prepare_if_state(
    claim: &Claim,
    generation: u64,
    submission: SubmissionId,
    expected: (StoreEpoch, u64),
) -> Result<Cancellation, Error> {
    if expected.0 != claim.binding().epoch() {
        return Err(Error::StateMismatch);
    }
    prepare_inner(claim, generation, submission, Some(expected.1), None)
}
fn prepare_inner(
    claim: &Claim,
    generation: u64,
    submission: SubmissionId,
    expected: Option<u64>,
    authorization: Option<Authorization>,
) -> Result<Cancellation, Error> {
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
        b"td-mta queue cancel v1".as_slice(),
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
    // Preserve unconditional request digests; a conditional request appends
    // one length-delimited field, distinct even when the expected state is zero.
    if let Some(expected) = expected {
        hash.update(&8u64.to_be_bytes())
            .map_err(|_| Error::Crypto)?;
        hash.update(&expected.to_be_bytes())
            .map_err(|_| Error::Crypto)?;
    }
    if let Some(authority) = &authorization {
        for field in [
            b"jmap-device-authorization-v1".as_slice(),
            authority.binding.profile().as_bytes(),
            authority.device.as_bytes().as_slice(),
        ] {
            hash.update(&(field.len() as u64).to_be_bytes())
                .map_err(|_| Error::Crypto)?;
            hash.update(field).map_err(|_| Error::Crypto)?;
        }
    }
    Ok(Cancellation {
        claim: claim.clone(),
        generation,
        submission,
        expected,
        authorization,
        operation: SubmissionCancelId::from_bytes(operation),
        digest: hash.finish().map_err(|_| Error::Crypto)?,
    })
}
fn number(bytes: &[u8]) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    ))
}
async fn receipt(client: &Client, value: &Cancellation) -> Result<Option<Outcome>, Error> {
    let rows=client.query("SELECT request_digest,submission_id,recipient_count,changed_count,completed_at,old_sequence,new_sequence FROM td_mta.submission_cancel_receipts WHERE account=$1 AND operation=$2", &[&value.claim.binding().account().as_bytes().as_slice(),&value.operation.as_bytes().as_slice()]).await.map_err(|_|Error::Database)?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let digest: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
    if digest != value.digest {
        return Err(Error::Conflict);
    }
    let submission: &[u8] = row.try_get(1).map_err(|_| Error::Integrity)?;
    let count: i64 = row.try_get(2).map_err(|_| Error::Integrity)?;
    let changed: i64 = row.try_get(3).map_err(|_| Error::Integrity)?;
    let completed: i64 = row.try_get(4).map_err(|_| Error::Integrity)?;
    let old = number(row.try_get(5).map_err(|_| Error::Integrity)?)?;
    let new = number(row.try_get(6).map_err(|_| Error::Integrity)?)?;
    if submission != value.submission.as_bytes()
        || !(1..=1000).contains(&count)
        || !(changed == 0 || changed == count)
        || old == 0
        || value.expected.is_some_and(|expected| expected != old)
        || (if changed == 0 {
            Some(old)
        } else {
            old.checked_add(1)
        }) != Some(new)
    {
        return Err(Error::Integrity);
    }
    Ok(Some(Outcome {
        submission: value.submission,
        recipients: u32::try_from(count).map_err(|_| Error::Integrity)?,
        changed: u32::try_from(changed).map_err(|_| Error::Integrity)?,
        completed_at: completed,
        old_sequence: old,
        new_sequence: new,
    }))
}
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Cancellation,
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

/// Locally abort/fence the owned route transport in `close`, without database
/// work or SMTP round trips.
/// It runs only after the entire mutation fits and its staging memory exists,
/// under the policy/account/route locks. It must obey the supplied deadline.
/// Receipt replay and already-canceled submissions do not invoke it.
/// This internal entry point does not establish client/operator authorization.
/// A fresh device-bound preparation requires `publish_authenticated`.
pub fn publish(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Cancellation,
    close: impl FnOnce(Instant) -> Result<(), Error> + 'static,
) -> Result<Outcome, Error> {
    publish_inner(request, trust, deadline, value, None, close)
}
/// Trusted client adapter: new publication requires a device-bound preparation
/// and a fresh credential. Known receipt settlement grants no new authority
/// and remains available without the original password after revocation.
/// The local-close contract of `publish` applies.
pub fn publish_authenticated(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Cancellation,
    password: &[u8],
    close: impl FnOnce(Instant) -> Result<(), Error> + 'static,
) -> Result<Outcome, Error> {
    if value.authorization.is_none() {
        return Err(Error::Policy);
    }
    publish_inner(request, trust, deadline, value, Some(password), close)
}
fn publish_inner(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Cancellation,
    password: Option<&[u8]>,
    close: impl FnOnce(Instant) -> Result<(), Error> + 'static,
) -> Result<Outcome, Error> {
    if let Some(outcome) = inspect(request, trust, deadline, value)? {
        return Ok(outcome);
    }
    let presented = if let Some(authority) = &value.authorization {
        let presented = password
            .and_then(Presented::parse)
            .ok_or(Error::Unauthenticated)?;
        if presented.id() != authority.device {
            return Err(Error::Unauthenticated);
        }
        Some(presented)
    } else {
        None
    };
    let value = value.clone();
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_cancellation_client(
        request,
        trust,
        deadline,
        move |client| {
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
                match (&value.authorization, presented) {
                    (Some(authority), Some(presented)) => {
                        client_policy(&value.claim, &authority.binding)?;
                        if postgres_devices::authenticate(
                            client,
                            authority.binding.account(),
                            presented,
                        )
                        .await
                        .map_err(Error::Credential)?
                            != Some(authority.device)
                        {
                            return Err(Error::Unauthenticated);
                        }
                    }
                    (None, None) => (),
                    _ => return Err(Error::Unauthenticated),
                }
                let now = owners::check_locked(client, &value.claim, value.generation)
                    .await
                    .map_err(Error::Owner)?;
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
                if old == 0 || number(state.try_get(1).map_err(|_| Error::Integrity)?)? > old {
                    return Err(Error::Integrity);
                }
                if value.expected.is_some_and(|expected| expected != old) {
                    return Err(Error::StateMismatch);
                }
                let count:i64 = client.query_one("SELECT count(*) FROM (SELECT 1 FROM td_mta.submission_cancel_receipts WHERE account=$1 LIMIT 1024) receipts", &[&account]).await.map_err(|_|Error::Database)?.try_get(0).map_err(|_|Error::Integrity)?;
                if count >= 1024 {
                    return Err(Error::Capacity);
                }
                let rows = client.query("SELECT s.email_id,s.thread_id,s.identity_id,s.transmitted_blob_id,s.reverse_path,s.send_at,s.expires_at,s.recipient_count,s.completed_at,s.notification,s.notification_email_id,s.changed FROM td_mta.submissions s JOIN td_mta.submission_routes r ON r.account=s.account AND r.submission_id=s.id WHERE s.account=$1 AND s.id=$2 AND r.route_name=$3 AND r.destination=$4 FOR UPDATE OF s", &[&account,&submission,&binding.route().name(),&binding.route().destination().as_slice()]).await.map_err(|_|Error::Database)?;
                let row = rows.first().ok_or(Error::Missing)?;
                let before = snapshot::submission(row)?;
                if number(row.try_get(11).map_err(|_| Error::Integrity)?)? > old {
                    return Err(Error::Integrity);
                }
                // Bound text before asking the driver to buffer the complete group.
                let totals = client.query_one("SELECT count(*),COALESCE(sum(octet_length(address)+COALESCE(octet_length(rcpt_reply),0)+COALESCE(octet_length(data_reply),0)+octet_length(diagnostic)),0)::bigint FROM td_mta.recipients WHERE account=$1 AND submission_id=$2", &[&account,&submission]).await.map_err(|_|Error::Database)?;
                let count: i64 = totals.try_get(0).map_err(|_| Error::Integrity)?;
                let text: i64 = totals.try_get(1).map_err(|_| Error::Integrity)?;
                if count != i64::from(before.recipient_count) {
                    return Err(Error::Integrity);
                }
                let budget = snapshot::budget(count, text)?;
                let recipients = client.query("SELECT ordinal,address,state,uncertain,attempt_id,attempt_count,last_attempt_at,phase,next_attempt_at,rcpt_reply,data_reply,reason,diagnostic,changed FROM td_mta.recipients WHERE account=$1 AND submission_id=$2 ORDER BY ordinal LIMIT 1001 FOR UPDATE", &[&account,&submission]).await.map_err(|_|Error::Database)?;
                let plan = snapshot::reserve(before, &recipients, old, now, budget)?;
                let next = if plan.changed {
                    old.checked_add(1).ok_or(Error::Capacity)?
                } else {
                    old
                };
                if Instant::now() >= deadline {
                    return Err(Error::Deadline);
                }
                if plan.changed {
                    close(deadline)?;
                }
                if Instant::now() >= deadline {
                    return Err(Error::Deadline);
                }
                owners::check_locked(client, &value.claim, value.generation)
                    .await
                    .map_err(Error::Owner)?;
                let changed = next.to_be_bytes();
                let changed = changed.as_slice();
                if plan.changed {
                    if client.execute("UPDATE td_mta.recipients SET state=6,uncertain=0,phase=CASE WHEN attempt_count=0 THEN 0 ELSE 4 END,next_attempt_at=NULL,reason=7,changed=$3 WHERE account=$1 AND submission_id=$2", &[&account,&submission,&changed]).await.map_err(|_|Error::Database)? != u64::from(before.recipient_count) { return Err(Error::Integrity); }
                    if client.execute("UPDATE td_mta.submissions SET changed=$3,completed_at=$4 WHERE account=$1 AND id=$2", &[&account,&submission,&changed,&plan.completed_at]).await.map_err(|_|Error::Database)? != 1 { return Err(Error::Integrity); }
                    client.execute("UPDATE td_mta.submission_attempts SET active=false WHERE account=$1 AND submission_id=$2 AND active", &[&account,&submission]).await.map_err(|_|Error::Database)?;
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
                }
                let changed_count = if plan.changed { count } else { 0 };
                client.execute("INSERT INTO td_mta.submission_cancel_receipts VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)", &[&account,&value.operation.as_bytes().as_slice(),&value.digest.as_slice(),&submission,&count,&changed_count,&plan.completed_at,&old.to_be_bytes().as_slice(),&changed]).await.map_err(|_|Error::Database)?;
                owners::check_locked(client, &value.claim, value.generation)
                    .await
                    .map_err(Error::Owner)?;
                if Instant::now() >= deadline {
                    return Err(Error::Deadline);
                }
                marker.store(true, Ordering::Relaxed);
                client
                    .batch_execute("COMMIT")
                    .await
                    .map_err(|_| Error::Indeterminate)?;
                // Keep the reserved, encoded mutation alive through settlement.
                drop(plan.bytes);
                Ok(Outcome {
                    submission: value.submission,
                    recipients: before.recipient_count,
                    changed: u32::try_from(changed_count).map_err(|_| Error::Integrity)?,
                    completed_at: plan.completed_at,
                    old_sequence: old,
                    new_sequence: next,
                })
            })
        },
    );
    match result {
        Ok(result) => result,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}
mod snapshot;
