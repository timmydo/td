//! Retained single-Email flag writes. No production JMAP dispatch is enabled.
use crate::{
    device_credentials::Presented,
    ids::{DeviceId, EmailId, FlagOperationId},
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Credential(postgres_devices::Error),
    Epoch,
    Policy,
    Unauthenticated,
    Missing,
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
/// Retain this exact value across uncertainty; it contains no password.
#[derive(Clone)]
pub struct Change {
    binding: Binding,
    operation: FlagOperationId,
    device: DeviceId,
    email: EmailId,
    expected: Option<u64>,
    seen: Option<bool>,
    flagged: Option<bool>,
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
        email,
        expected,
        seen,
        flagged,
        digest: hash.finish().map_err(|_| Error::Crypto)?,
    })
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
async fn receipt(client: &Client, change: &Change) -> Result<Option<Outcome>, Error> {
    let rows = client.query("SELECT request_digest,old_sequence,new_sequence FROM td_mta.flag_receipts WHERE account=$1 AND operation=$2 LIMIT 2",
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
    Ok(Some(Outcome {
        old_sequence,
        new_sequence,
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
fn finish_inspection(
    result: Result<Option<Outcome>, Error>,
    deadline: Instant,
) -> Result<Option<Outcome>, Error> {
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
    if let Some(outcome) = inspect(request, trust, deadline, change)? {
        return Ok(outcome);
    }
    let presented = Presented::parse(password).ok_or(Error::Unauthenticated)?;
    if presented.id() != change.device {
        return Err(Error::Unauthenticated);
    }
    let change = change.clone();
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_mailbox_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            begin(client, &change).await?;
            let selected = client.query_one("SELECT generation,version_id FROM td_mta.config_selection WHERE singleton=true FOR SHARE", &[]).await.map_err(|_| Error::Database)?;
            let generation = sequence(selected.try_get(0).map_err(|_| Error::Integrity)?)?;
            let version: Option<&[u8]> = selected.try_get(1).map_err(|_| Error::Integrity)?;
            // Hold policy first even while waiting for the account fence.
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
            let rows = client.query("SELECT e.changed,s.changed,f.changed FROM td_mta.emails e LEFT JOIN td_mta.keywords s ON s.account=e.account AND s.email_id=e.id AND s.keyword='$seen' LEFT JOIN td_mta.keywords f ON f.account=e.account AND f.email_id=e.id AND f.keyword='$flagged' WHERE e.account=$1 AND e.id=$2 LIMIT 2",
            &[&account.as_bytes().as_slice(), &change.email.as_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
            let row = rows.first().ok_or(Error::Missing)?;
            if rows.len() != 1 || sequence(row.try_get(0).map_err(|_| Error::Integrity)?)? > old {
                return Err(Error::Integrity);
            }
            let seen: Option<&[u8]> = row.try_get(1).map_err(|_| Error::Integrity)?;
            let flagged: Option<&[u8]> = row.try_get(2).map_err(|_| Error::Integrity)?;
            for value in [seen, flagged].into_iter().flatten() {
                if sequence(value)? > old {
                    return Err(Error::Integrity);
                }
            }
            let seen_changed = change.seen.is_some_and(|value| value != seen.is_some());
            let flagged_changed = change
                .flagged
                .is_some_and(|value| value != flagged.is_some());
            let next = if seen_changed || flagged_changed {
                old.checked_add(1).ok_or(Error::Capacity)?
            } else {
                old
            };
            if next != old {
                if seen_changed {
                    let rows = client.query("SELECT m.changed,b.changed FROM td_mta.memberships m LEFT JOIN td_mta.mailboxes b ON b.account=m.account AND b.id=m.mailbox_id WHERE m.account=$1 AND m.email_id=$2 LIMIT 257", &[&account.as_bytes().as_slice(), &change.email.as_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
                    if rows.len() > 256 {
                        return Err(Error::Capacity);
                    }
                    for row in rows {
                        for column in [0usize, 1] {
                            if sequence(row.try_get(column).map_err(|_| Error::Integrity)?)? > old {
                                return Err(Error::Integrity);
                            }
                        }
                    }
                }
                for (keyword, value, changed) in [
                    ("$seen", change.seen, seen_changed),
                    ("$flagged", change.flagged, flagged_changed),
                ] {
                    if changed {
                        match value {
                            Some(true) => {
                                client.execute("INSERT INTO td_mta.keywords(account,email_id,keyword,changed) VALUES($1,$2,$3,$4)", &[&account.as_bytes().as_slice(), &change.email.as_bytes().as_slice(), &keyword, &next.to_be_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
                            }
                            Some(false) => {
                                client.execute("DELETE FROM td_mta.keywords WHERE account=$1 AND email_id=$2 AND keyword=$3", &[&account.as_bytes().as_slice(), &change.email.as_bytes().as_slice(), &keyword]).await.map_err(|_| Error::Database)?;
                            }
                            None => return Err(Error::Integrity),
                        }
                    }
                }
                client
                    .execute(
                        "UPDATE td_mta.emails SET changed=$3 WHERE account=$1 AND id=$2",
                        &[
                            &account.as_bytes().as_slice(),
                            &change.email.as_bytes().as_slice(),
                            &next.to_be_bytes().as_slice(),
                        ],
                    )
                    .await
                    .map_err(|_| Error::Database)?;
                client.execute("INSERT INTO td_mta.changes(account,sequence,operation,kind,action,object) VALUES($1,$3,0,3,2,$2)", &[&account.as_bytes().as_slice(), &change.email.as_bytes().as_slice(), &next.to_be_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
                if seen_changed {
                    client.execute("WITH changed AS (UPDATE td_mta.mailboxes b SET changed=$3 FROM td_mta.memberships m WHERE m.account=$1 AND m.email_id=$2 AND b.account=m.account AND b.id=m.mailbox_id RETURNING b.id) INSERT INTO td_mta.changes(account,sequence,operation,kind,action,object) SELECT $1,$3,row_number() OVER (ORDER BY id),1,2,id FROM changed", &[&account.as_bytes().as_slice(), &change.email.as_bytes().as_slice(), &next.to_be_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
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
            client.execute("INSERT INTO td_mta.flag_receipts(account,operation,request_digest,old_sequence,new_sequence) VALUES($1,$2,$3,$4,$5)", &[&account.as_bytes().as_slice(), &change.operation.as_bytes().as_slice(), &change.digest.as_slice(), &old.to_be_bytes().as_slice(), &next.to_be_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
            marker.store(true, Ordering::Relaxed);
            client
                .batch_execute("COMMIT")
                .await
                .map_err(|_| Error::Indeterminate)?;
            Ok(Outcome {
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
            finish_inspection(Ok(None), deadline),
            Err(Error::Connection(postgres_probe::Error::Deadline))
        );
        assert_eq!(
            finish_inspection(Err(Error::Database), deadline),
            Err(Error::Connection(postgres_probe::Error::Deadline))
        );
    }
}
