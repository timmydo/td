//! Retained bounded Email flag and mailbox writes. No production JMAP dispatch is enabled.
use crate::{
    device_credentials::Presented,
    ids::{DeviceId, EmailId, FlagOperationId, MailboxId},
    postgres_devices,
    postgres_probe::{self, Request},
    postgres_runtime_profile::Binding,
    postgres_schema,
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

pub const MAX_RECEIPTS: i64 = 1024;
pub const MAX_UPDATES: usize = 256;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Credential(postgres_devices::Error),
    Epoch,
    Policy,
    Unauthenticated,
    Missing,
    Invalid,
    State,
    Capacity,
    Integrity,
    Conflict,
    Crypto,
    Database,
    Indeterminate,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Outcome {
    pub old_sequence: u64,
    pub new_sequence: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Update {
    /// Replace all memberships with one existing account mailbox.
    pub mailbox: Option<MailboxId>,
    pub email: EmailId,
    pub seen: Option<bool>,
    pub flagged: Option<bool>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateResult {
    Updated,
    NotFound,
    InvalidMailbox,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchOutcome {
    pub state: Outcome,
    pub results: Vec<UpdateResult>,
}
/// Retain this exact value across uncertainty; it contains no password.
#[derive(Clone)]
pub struct Change {
    binding: Binding,
    operation: FlagOperationId,
    device: DeviceId,
    updates: Vec<Update>,
    single: bool,
    expected: Option<u64>,
    digest: [u8; 32],
}
impl Change {
    pub fn operation(&self) -> FlagOperationId {
        self.operation
    }
}

pub fn prepare(
    binding: &Binding,
    device: DeviceId,
    email: EmailId,
    expected: Option<u64>,
    seen: Option<bool>,
    flagged: Option<bool>,
) -> Result<Change, Error> {
    if !binding.roles().jmap() {
        return Err(Error::Policy);
    }
    let mut id = [0; 16];
    SystemEntropy::try_new()
        .and_then(|mut rng| rng.fill(&mut id))
        .map_err(|_| Error::Crypto)?;
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    let selected = binding.selection();
    let options = [
        u8::from(expected.is_some()),
        flag(seen),
        flag(flagged),
        binding.roles().bits(),
    ];
    for part in [
        b"td-mta PostgreSQL flags v1\0".as_slice(),
        binding.epoch().as_bytes().as_slice(),
        binding.account().as_bytes().as_slice(),
        &selected.generation.to_be_bytes(),
        selected.version.as_bytes().as_slice(),
        device.as_bytes().as_slice(),
        email.as_bytes().as_slice(),
        &expected.unwrap_or(0).to_be_bytes(),
        &options,
        binding.profile().as_bytes(),
    ] {
        hash.update(part).map_err(|_| Error::Crypto)?;
    }
    Ok(Change {
        binding: binding.clone(),
        operation: FlagOperationId::from_bytes(id),
        device,
        updates: copy_updates(&[Update {
            mailbox: None,
            email,
            seen,
            flagged,
        }])?,
        single: true,
        expected,
        digest: hash.finish().map_err(|_| Error::Crypto)?,
    })
}

/// Prepare a complete batch once, retaining request order and one operation ID.
/// Duplicate Email IDs refuse before publication. Missing Emails are per-object
/// NotFound results; an empty batch records a no-op result.
pub fn prepare_batch(
    binding: &Binding,
    device: DeviceId,
    updates: &[Update],
    expected: Option<u64>,
) -> Result<Change, Error> {
    if !binding.roles().jmap() {
        return Err(Error::Policy);
    }
    let updates = copy_updates(updates)?;
    for (index, update) in updates.iter().enumerate() {
        if updates
            .get(..index)
            .ok_or(Error::Invalid)?
            .iter()
            .any(|prior| prior.email == update.email)
        {
            return Err(Error::Invalid);
        }
    }
    let mut id = [0; 16];
    SystemEntropy::try_new()
        .and_then(|mut rng| rng.fill(&mut id))
        .map_err(|_| Error::Crypto)?;
    let selected = binding.selection();
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    for part in [
        b"td-mta PostgreSQL Email update batch v2\0".as_slice(),
        binding.epoch().as_bytes().as_slice(),
        binding.account().as_bytes().as_slice(),
        &selected.generation.to_be_bytes(),
        selected.version.as_bytes().as_slice(),
        device.as_bytes().as_slice(),
        &[u8::from(expected.is_some()), binding.roles().bits()],
        &expected.unwrap_or(0).to_be_bytes(),
        &(binding.profile().len() as u64).to_be_bytes(),
        binding.profile().as_bytes(),
        &(updates.len() as u64).to_be_bytes(),
    ] {
        hash.update(part).map_err(|_| Error::Crypto)?;
    }
    for update in &updates {
        hash.update(update.email.as_bytes())
            .map_err(|_| Error::Crypto)?;
        hash.update(&[
            flag(update.seen),
            flag(update.flagged),
            u8::from(update.mailbox.is_some()),
        ])
        .map_err(|_| Error::Crypto)?;
        if let Some(mailbox) = update.mailbox {
            hash.update(mailbox.as_bytes()).map_err(|_| Error::Crypto)?;
        }
    }
    Ok(Change {
        binding: binding.clone(),
        operation: FlagOperationId::from_bytes(id),
        device,
        updates,
        single: false,
        expected,
        digest: hash.finish().map_err(|_| Error::Crypto)?,
    })
}
fn copy_updates(updates: &[Update]) -> Result<Vec<Update>, Error> {
    if updates.len() > MAX_UPDATES {
        return Err(Error::Capacity);
    }
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(updates.len())
        .map_err(|_| Error::Capacity)?;
    owned.extend_from_slice(updates);
    Ok(owned)
}
fn single(outcome: BatchOutcome) -> Result<Outcome, Error> {
    if outcome.results.as_slice() != [UpdateResult::Updated] {
        return Err(Error::Integrity);
    }
    Ok(outcome.state)
}

fn flag(value: Option<bool>) -> u8 {
    match value {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    }
}
fn sequence(bytes: &[u8]) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    ))
}
async fn begin(client: &Client, change: &Change) -> Result<(), Error> {
    let schema = postgres_schema::lock_and_inspect(client)
        .await
        .map_err(Error::Schema)?;
    if schema.store_epoch != change.binding.epoch() {
        return Err(Error::Epoch);
    }
    client.batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_| Error::Database)
}
async fn receipt(client: &Client, change: &Change) -> Result<Option<BatchOutcome>, Error> {
    let rows = client.query("SELECT request_digest,old_sequence,new_sequence,results FROM td_mta.flag_receipts WHERE account=$1 AND operation=$2 LIMIT 2",
        &[&change.binding.account().as_bytes().as_slice(), &change.operation.as_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    if rows.len() != 1 {
        return Err(Error::Integrity);
    }
    let digest: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
    if digest != change.digest {
        return Err(Error::Conflict);
    }
    let old_sequence = sequence(row.try_get(1).map_err(|_| Error::Integrity)?)?;
    let new_sequence = sequence(row.try_get(2).map_err(|_| Error::Integrity)?)?;
    if new_sequence != old_sequence && Some(new_sequence) != old_sequence.checked_add(1) {
        return Err(Error::Integrity);
    }
    let bytes: &[u8] = row.try_get(3).map_err(|_| Error::Integrity)?;
    if bytes.len() != change.updates.len() {
        return Err(Error::Integrity);
    }
    let mut results = Vec::new();
    results
        .try_reserve_exact(bytes.len())
        .map_err(|_| Error::Capacity)?;
    for (byte, update) in bytes.iter().zip(&change.updates) {
        results.push(match byte {
            1 => UpdateResult::Updated,
            2 if !change.single => UpdateResult::NotFound,
            3 if !change.single && update.mailbox.is_some() => UpdateResult::InvalidMailbox,
            _ => return Err(Error::Integrity),
        });
    }
    Ok(Some(BatchOutcome {
        state: Outcome {
            old_sequence,
            new_sequence,
        },
        results,
    }))
}
/// Owner-side recovery only. Wait behind any prior account writer before
/// checking its receipt. Failure leaves prior uncertainty unresolved. A receipt
/// proves the historical result, even after policy change or device revocation.
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    change: &Change,
) -> Result<Option<Outcome>, Error> {
    if !change.single {
        return Err(Error::Invalid);
    }
    inspect_record(request, trust, deadline, change)?
        .map(single)
        .transpose()
}
pub fn inspect_batch(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    change: &Change,
) -> Result<Option<BatchOutcome>, Error> {
    if change.single {
        return Err(Error::Invalid);
    }
    inspect_record(request, trust, deadline, change)
}
fn inspect_record(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    change: &Change,
) -> Result<Option<BatchOutcome>, Error> {
    let change = change.clone();
    let result = postgres_probe::with_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            begin(client, &change).await?;
            client
                .query_one(
                    "SELECT id FROM td_mta.accounts WHERE id=$1 FOR SHARE",
                    &[&change.binding.account().as_bytes().as_slice()],
                )
                .await
                .map_err(|_| Error::Database)?;
            receipt(client, &change).await
        })
    })
    .map_err(Error::Connection)?;
    finish_inspection(result, deadline)
}
fn finish_inspection<T>(
    result: Result<Option<T>, Error>,
    deadline: Instant,
) -> Result<Option<T>, Error> {
    // A confirmed historical success must survive a late final local poll.
    if !matches!(result, Ok(Some(_))) && Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}
/// Assign flags to one Email. A retained historical receipt succeeds without
/// reauthorizing a new write. Fresh writes fence policy, account, then device;
/// missing objects and state mismatch roll back without a receipt.
pub fn publish(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    change: &Change,
    password: &[u8],
) -> Result<Outcome, Error> {
    if !change.single {
        return Err(Error::Invalid);
    }
    single(publish_record(request, trust, deadline, change, password)?)
}
pub fn publish_batch(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    change: &Change,
    password: &[u8],
) -> Result<BatchOutcome, Error> {
    if change.single {
        return Err(Error::Invalid);
    }
    publish_record(request, trust, deadline, change, password)
}
struct Plan {
    mailbox: Option<MailboxId>,
    insert_target: bool,
    removals: u64,
    email: EmailId,
    seen: Option<bool>,
    flagged: Option<bool>,
}
fn ids(plans: &[Plan], include: impl Fn(&Plan) -> bool) -> Result<Vec<&[u8]>, Error> {
    let mut ids = Vec::new();
    ids.try_reserve_exact(plans.len())
        .map_err(|_| Error::Capacity)?;
    for plan in plans {
        if include(plan) {
            ids.push(plan.email.as_bytes().as_slice());
        }
    }
    Ok(ids)
}
fn publish_record(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    change: &Change,
    password: &[u8],
) -> Result<BatchOutcome, Error> {
    if let Some(outcome) = inspect_record(request, trust, deadline, change)? {
        return Ok(outcome);
    }
    let presented = Presented::parse(password).ok_or(Error::Unauthenticated)?;
    if presented.id() != change.device {
        return Err(Error::Unauthenticated);
    }
    let change = change.clone();
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_mutation_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            begin(client, &change).await?;
            let selected = client.query_one("SELECT generation,version_id FROM td_mta.config_selection WHERE singleton=true FOR SHARE", &[]).await.map_err(|_| Error::Database)?;
            let generation = sequence(selected.try_get(0).map_err(|_| Error::Integrity)?)?;
            let version: Option<&[u8]> = selected.try_get(1).map_err(|_| Error::Integrity)?;
            let account = change.binding.account();
            let row = client
                .query_one(
                    "SELECT sequence,floor FROM td_mta.accounts WHERE id=$1 FOR UPDATE",
                    &[&account.as_bytes().as_slice()],
                )
                .await
                .map_err(|_| Error::Database)?;
            if let Some(outcome) = receipt(client, &change).await? {
                return Ok(outcome);
            }
            if generation != change.binding.selection().generation
                || version != Some(change.binding.selection().version.as_bytes().as_slice())
                || !change.binding.roles().jmap()
            {
                return Err(Error::Policy);
            }
            if postgres_devices::authenticate(client, account, presented)
                .await
                .map_err(Error::Credential)?
                != Some(change.device)
            {
                return Err(Error::Unauthenticated);
            }
            let old = sequence(row.try_get(0).map_err(|_| Error::Integrity)?)?;
            let floor = sequence(row.try_get(1).map_err(|_| Error::Integrity)?)?;
            if floor > old {
                return Err(Error::Integrity);
            }
            if change.expected.is_some_and(|expected| expected != old) {
                return Err(Error::State);
            }
            let count: i64 = client.query_one("SELECT count(*) FROM (SELECT 1 FROM td_mta.flag_receipts WHERE account=$1 LIMIT $2) bounded", &[&account.as_bytes().as_slice(), &MAX_RECEIPTS]).await.map_err(|_| Error::Database)?.try_get(0).map_err(|_| Error::Integrity)?;
            if count >= MAX_RECEIPTS {
                return Err(Error::Capacity);
            }
            let mut requested = Vec::new();
            requested
                .try_reserve_exact(change.updates.len())
                .map_err(|_| Error::Capacity)?;
            let mut targets = Vec::new();
            targets
                .try_reserve_exact(change.updates.len())
                .map_err(|_| Error::Capacity)?;
            for update in &change.updates {
                requested.push(update.email.as_bytes().as_slice());
                targets.push(update.mailbox.as_ref().map(|id| id.as_bytes().as_slice()));
            }
            let rows = client.query("SELECT q.ordinality,e.changed,s.changed,f.changed,b.changed,m.n,m.present,m.valid FROM unnest($2::bytea[],$3::bytea[]) WITH ORDINALITY q(id,target,ordinality) LEFT JOIN td_mta.emails e ON e.account=$1 AND e.id=q.id LEFT JOIN td_mta.keywords s ON s.account=e.account AND s.email_id=e.id AND s.keyword='$seen' LEFT JOIN td_mta.keywords f ON f.account=e.account AND f.email_id=e.id AND f.keyword='$flagged' LEFT JOIN td_mta.mailboxes b ON b.account=e.account AND b.id=q.target LEFT JOIN LATERAL (SELECT count(*) n,coalesce(bool_or(v.mailbox_id=q.target),false) present,coalesce(bool_and(v.changed<=$4 AND v.mailbox_changed<=$4),true) valid FROM (SELECT m.mailbox_id,m.changed,b.changed mailbox_changed FROM td_mta.memberships m JOIN td_mta.mailboxes b ON b.account=m.account AND b.id=m.mailbox_id WHERE m.account=e.account AND m.email_id=e.id AND q.target IS NOT NULL LIMIT 257) v) m ON true ORDER BY q.ordinality", &[&account.as_bytes().as_slice(), &requested, &targets, &old.to_be_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
            if rows.len() != change.updates.len() {
                return Err(Error::Integrity);
            }
            let mut plans = Vec::new();
            let mut results = Vec::new();
            plans
                .try_reserve_exact(rows.len())
                .map_err(|_| Error::Capacity)?;
            results
                .try_reserve_exact(rows.len())
                .map_err(|_| Error::Capacity)?;
            for (index, (row, update)) in rows.iter().zip(&change.updates).enumerate() {
                let ordinal: i64 = row.try_get(0).map_err(|_| Error::Integrity)?;
                if usize::try_from(ordinal).ok() != Some(index + 1) {
                    return Err(Error::Integrity);
                }
                let changed: Option<&[u8]> = row.try_get(1).map_err(|_| Error::Integrity)?;
                let seen: Option<&[u8]> = row.try_get(2).map_err(|_| Error::Integrity)?;
                let flagged: Option<&[u8]> = row.try_get(3).map_err(|_| Error::Integrity)?;
                let Some(changed) = changed else {
                    if seen.is_some() || flagged.is_some() {
                        return Err(Error::Integrity);
                    }
                    if change.single {
                        return Err(Error::Missing);
                    }
                    results.push(UpdateResult::NotFound);
                    continue;
                };
                for value in [Some(changed), seen, flagged].into_iter().flatten() {
                    if sequence(value)? > old {
                        return Err(Error::Integrity);
                    }
                }
                let mut mailbox = None;
                let mut insert_target = false;
                let mut removals = 0;
                if let Some(target) = update.mailbox {
                    let target_changed: Option<&[u8]> =
                        row.try_get(4).map_err(|_| Error::Integrity)?;
                    let Some(target_changed) = target_changed else {
                        results.push(UpdateResult::InvalidMailbox);
                        continue;
                    };
                    let count: i64 = row.try_get(5).map_err(|_| Error::Integrity)?;
                    let count = u64::try_from(count).map_err(|_| Error::Integrity)?;
                    let present: bool = row.try_get(6).map_err(|_| Error::Integrity)?;
                    let valid: bool = row.try_get(7).map_err(|_| Error::Integrity)?;
                    if !valid || sequence(target_changed)? > old {
                        return Err(Error::Integrity);
                    }
                    if count > crate::postgres_mailboxes::MAX_MAILBOXES as u64 {
                        return Err(Error::Capacity);
                    }
                    if count != 1 || !present {
                        mailbox = Some(target);
                        insert_target = !present;
                        removals = count
                            .checked_sub(u64::from(present))
                            .ok_or(Error::Integrity)?;
                    }
                }
                results.push(UpdateResult::Updated);
                let seen = update.seen.filter(|value| *value != seen.is_some());
                let flagged = update.flagged.filter(|value| *value != flagged.is_some());
                if seen.is_some() || flagged.is_some() || mailbox.is_some() {
                    plans.push(Plan {
                        mailbox,
                        insert_target,
                        removals,
                        email: update.email,
                        seen,
                        flagged,
                    });
                }
            }
            let next = if plans.is_empty() {
                old
            } else {
                old.checked_add(1).ok_or(Error::Capacity)?
            };
            if next != old {
                let changed = ids(&plans, |_| true)?;
                let mut targets = Vec::new();
                let mut seen_changes = Vec::new();
                let mut inserts = Vec::new();
                targets
                    .try_reserve_exact(plans.len())
                    .map_err(|_| Error::Capacity)?;
                seen_changes
                    .try_reserve_exact(plans.len())
                    .map_err(|_| Error::Capacity)?;
                inserts
                    .try_reserve_exact(plans.len())
                    .map_err(|_| Error::Capacity)?;
                let mut removal_count = 0u64;
                for plan in &plans {
                    targets.push(plan.mailbox.as_ref().map(|id| id.as_bytes().as_slice()));
                    seen_changes.push(plan.seen.is_some());
                    inserts.push(plan.insert_target);
                    removal_count = removal_count
                        .checked_add(plan.removals)
                        .ok_or(Error::Capacity)?;
                }
                let rows = client.query("WITH affected AS (SELECT m.mailbox_id,m.changed FROM unnest($2::bytea[],$3::bytea[],$4::bool[]) q(id,target,seen) JOIN td_mta.memberships m ON m.account=$1 AND m.email_id=q.id WHERE q.seen OR (q.target IS NOT NULL AND m.mailbox_id<>q.target) UNION ALL SELECT target,$6 FROM unnest($3::bytea[],$5::bool[]) q(target,insert_target) WHERE insert_target) SELECT a.mailbox_id,b.changed,bool_and(a.changed<=$6) FROM affected a LEFT JOIN td_mta.mailboxes b ON b.account=$1 AND b.id=a.mailbox_id GROUP BY a.mailbox_id,b.changed ORDER BY a.mailbox_id LIMIT 257", &[&account.as_bytes().as_slice(), &changed, &targets, &seen_changes, &inserts, &old.to_be_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
                if rows.len() > crate::postgres_mailboxes::MAX_MAILBOXES {
                    return Err(Error::Capacity);
                }
                let mut mailboxes = Vec::new();
                mailboxes
                    .try_reserve_exact(rows.len())
                    .map_err(|_| Error::Capacity)?;
                for row in &rows {
                    let valid: bool = row.try_get(2).map_err(|_| Error::Integrity)?;
                    if !valid || sequence(row.try_get(1).map_err(|_| Error::Integrity)?)? > old {
                        return Err(Error::Integrity);
                    }
                    let id: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
                    if id.len() != 16 {
                        return Err(Error::Integrity);
                    }
                    mailboxes.push(id);
                }
                let written = client.execute("DELETE FROM td_mta.memberships m USING unnest($2::bytea[],$3::bytea[]) q(id,target) WHERE m.account=$1 AND m.email_id=q.id AND q.target IS NOT NULL AND m.mailbox_id<>q.target", &[&account.as_bytes().as_slice(), &changed, &targets]).await.map_err(|_| Error::Database)?;
                if written != removal_count {
                    return Err(Error::Integrity);
                }
                let written = client.execute("INSERT INTO td_mta.memberships(account,email_id,mailbox_id,changed) SELECT $1,id,target,$5 FROM unnest($2::bytea[],$3::bytea[],$4::bool[]) q(id,target,insert_target) WHERE insert_target", &[&account.as_bytes().as_slice(), &changed, &targets, &inserts, &next.to_be_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
                if written != inserts.iter().filter(|value| **value).count() as u64 {
                    return Err(Error::Integrity);
                }
                for (keyword, seen) in [("$seen", true), ("$flagged", false)] {
                    for desired in [true, false] {
                        let selected = ids(&plans, |plan| {
                            (if seen { plan.seen } else { plan.flagged }) == Some(desired)
                        })?;
                        if selected.is_empty() {
                            continue;
                        }
                        let written = if desired {
                        client.execute("INSERT INTO td_mta.keywords(account,email_id,keyword,changed) SELECT $1,id,$3,$4 FROM unnest($2::bytea[]) q(id)", &[&account.as_bytes().as_slice(), &selected, &keyword, &next.to_be_bytes().as_slice()]).await
                    } else {
                        client.execute("DELETE FROM td_mta.keywords WHERE account=$1 AND email_id=ANY($2::bytea[]) AND keyword=$3", &[&account.as_bytes().as_slice(), &selected, &keyword]).await
                    }.map_err(|_| Error::Database)?;
                        if written != selected.len() as u64 {
                            return Err(Error::Integrity);
                        }
                    }
                }
                let written = client.execute("UPDATE td_mta.emails SET changed=$3 WHERE account=$1 AND id=ANY($2::bytea[])", &[&account.as_bytes().as_slice(), &changed, &next.to_be_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
                if written != changed.len() as u64 {
                    return Err(Error::Integrity);
                }
                client.execute("INSERT INTO td_mta.changes(account,sequence,operation,kind,action,object) SELECT $1,$3,ordinality-1,3,2,id FROM unnest($2::bytea[]) WITH ORDINALITY q(id,ordinality)", &[&account.as_bytes().as_slice(), &changed, &next.to_be_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
                if !mailboxes.is_empty() {
                    let offset = i64::try_from(changed.len()).map_err(|_| Error::Capacity)?;
                    let written = client.execute("WITH changed AS (UPDATE td_mta.mailboxes SET changed=$3 WHERE account=$1 AND id=ANY($2::bytea[]) RETURNING id) INSERT INTO td_mta.changes(account,sequence,operation,kind,action,object) SELECT $1,$3,row_number() OVER (ORDER BY id)+$4-1,1,2,id FROM changed", &[&account.as_bytes().as_slice(), &mailboxes, &next.to_be_bytes().as_slice(), &offset]).await.map_err(|_| Error::Database)?;
                    if written != mailboxes.len() as u64 {
                        return Err(Error::Integrity);
                    }
                }
                client
                    .execute(
                        "UPDATE td_mta.accounts SET sequence=$2 WHERE id=$1",
                        &[
                            &account.as_bytes().as_slice(),
                            &next.to_be_bytes().as_slice(),
                        ],
                    )
                    .await
                    .map_err(|_| Error::Database)?;
            }
            let mut encoded = Vec::new();
            encoded
                .try_reserve_exact(results.len())
                .map_err(|_| Error::Capacity)?;
            for result in &results {
                encoded.push(match result {
                    UpdateResult::Updated => 1u8,
                    UpdateResult::NotFound => 2,
                    UpdateResult::InvalidMailbox => 3,
                });
            }
            client.execute("INSERT INTO td_mta.flag_receipts(account,operation,request_digest,old_sequence,new_sequence,results) VALUES($1,$2,$3,$4,$5,$6)", &[&account.as_bytes().as_slice(), &change.operation.as_bytes().as_slice(), &change.digest.as_slice(), &old.to_be_bytes().as_slice(), &next.to_be_bytes().as_slice(), &encoded]).await.map_err(|_| Error::Database)?;
            marker.store(true, Ordering::Relaxed);
            client
                .batch_execute("COMMIT")
                .await
                .map_err(|_| Error::Indeterminate)?;
            Ok(BatchOutcome {
                state: Outcome {
                    old_sequence: old,
                    new_sequence: next,
                },
                results,
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
mod tests {
    use super::*;
    #[test]
    fn late_inspection_keeps_confirmed_success_but_refuses_absence() {
        let deadline = Instant::now();
        let outcome = Outcome {
            old_sequence: 4,
            new_sequence: 5,
        };
        assert_eq!(
            finish_inspection(Ok(Some(outcome)), deadline),
            Ok(Some(outcome))
        );
        assert_eq!(
            finish_inspection::<Outcome>(Ok(None), deadline),
            Err(Error::Connection(postgres_probe::Error::Deadline))
        );
        assert_eq!(
            finish_inspection::<Outcome>(Err(Error::Database), deadline),
            Err(Error::Connection(postgres_probe::Error::Deadline))
        );
    }
}
