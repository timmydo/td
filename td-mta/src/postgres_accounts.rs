//! Explicit owner-side account bootstrap; no configuration or serving admission.
use crate::{
    format::{row::INITIAL_INBOX, ObjectType},
    ids::{AccountId, MailboxId, StoreEpoch},
    postgres_probe::{self, Request},
    postgres_schema,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
use td_crypto::{Entropy, SystemEntropy, TrustStore};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Schema(postgres_schema::Error),
    Connection(postgres_probe::Error),
    Epoch,
    Missing,
    Exists,
    Conflict,
    Integrity,
    Crypto,
    Database,
    /// Retain the creation and inspect/retry it after the original settles.
    Indeterminate,
}

/// Retained identity of one bootstrap. Not a device or policy authorization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Creation {
    epoch: StoreEpoch,
    account: AccountId,
    inbox: MailboxId,
}
impl Creation {
    pub fn epoch(self) -> StoreEpoch {
        self.epoch
    }
    pub fn account(self) -> AccountId {
        self.account
    }
    pub fn inbox(self) -> MailboxId {
        self.inbox
    }
}

pub fn prepare(epoch: StoreEpoch, account: AccountId) -> Result<Creation, Error> {
    let mut bytes = [0; 16];
    SystemEntropy::try_new()
        .and_then(|mut rng| rng.fill(&mut bytes))
        .map_err(|_| Error::Crypto)?;
    Ok(Creation {
        epoch,
        account,
        inbox: MailboxId::from_bytes(bytes),
    })
}

/// Atomically create account sequence one, Inbox and its Created history row.
/// Matching retained receipts succeed without touching later mailbox/state
/// changes. An existing account without this receipt is never repaired/reset.
pub fn publish(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    creation: Creation,
) -> Result<(), Error> {
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let receipt = postgres_schema::lock_and_inspect(client)
                .await
                .map_err(Error::Schema)?;
            if receipt.store_epoch != creation.epoch {
                return Err(Error::Epoch);
            }
            client.batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit = on; SET LOCAL idle_in_transaction_session_timeout = 10000").await.map_err(|_| Error::Database)?;
            let account = creation.account.as_bytes().as_slice();
            let inbox = creation.inbox.as_bytes().as_slice();
            let first = 1u64.to_be_bytes();
            let floor = 0u64.to_be_bytes();
            let inserted = client.execute("INSERT INTO td_mta.accounts (id,sequence,floor) VALUES ($1,$2,$3) ON CONFLICT (id) DO NOTHING", &[&account, &first.as_slice(), &floor.as_slice()]).await.map_err(|_| Error::Database)?;
            if inserted == 0 {
                let rows = client
                    .query(
                        "SELECT inbox FROM td_mta.account_bootstraps WHERE account=$1 LIMIT 2",
                        &[&account],
                    )
                    .await
                    .map_err(|_| Error::Database)?;
                if rows.is_empty() {
                    return Err(Error::Exists);
                }
                if rows.len() != 1 {
                    return Err(Error::Integrity);
                }
                let stored: &[u8] = rows
                    .first()
                    .ok_or(Error::Integrity)?
                    .try_get(0)
                    .map_err(|_| Error::Integrity)?;
                if stored != inbox {
                    return Err(Error::Conflict);
                }
                return Ok(());
            }
            if inserted != 1 {
                return Err(Error::Integrity);
            }
            let parent = INITIAL_INBOX
                .parent
                .as_ref()
                .map(|id| id.as_bytes().as_slice());
            let changed = client.execute("INSERT INTO td_mta.mailboxes (account,id,name,parent_id,role,sort_order,subscribed,changed) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)", &[&account, &inbox, &INITIAL_INBOX.name, &parent, &INITIAL_INBOX.role, &i64::from(INITIAL_INBOX.sort_order), &i64::from(INITIAL_INBOX.subscribed), &first.as_slice()]).await.map_err(|_| Error::Database)?;
            if changed != 1 {
                return Err(Error::Integrity);
            }
            let changed = client.execute("INSERT INTO td_mta.changes (account,sequence,operation,kind,action,object) VALUES ($1,$2,1,$3,1,$4)", &[&account, &first.as_slice(), &i64::from(ObjectType::Mailbox.tag()), &inbox]).await.map_err(|_| Error::Database)?;
            if changed != 1 {
                return Err(Error::Integrity);
            }
            let changed = client
                .execute(
                    "INSERT INTO td_mta.account_bootstraps (account,inbox) VALUES ($1,$2)",
                    &[&account, &inbox],
                )
                .await
                .map_err(|_| Error::Database)?;
            if changed != 1 {
                return Err(Error::Integrity);
            }
            marker.store(true, Ordering::Relaxed);
            client
                .batch_execute("COMMIT")
                .await
                .map_err(|_| Error::Indeterminate)?;
            Ok(())
        })
    });
    match result {
        Ok(result) => result,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}

/// Inspect the retained creation receipt, not current mailbox contents or
/// configuration authorization. A missing receipt cannot settle an in-flight
/// creation; the original operation must finish before interpreting absence.
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    account: AccountId,
) -> Result<Creation, Error> {
    let result = postgres_probe::with_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let receipt = postgres_schema::lock_and_inspect(client).await.map_err(Error::Schema)?;
            if receipt.store_epoch != epoch { return Err(Error::Epoch); }
            client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout = 10000").await.map_err(|_| Error::Database)?;
            let rows = client.query("SELECT inbox FROM td_mta.account_bootstraps WHERE account=$1 LIMIT 2", &[&account.as_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
            if rows.is_empty() { return Err(Error::Missing); }
            if rows.len() != 1 { return Err(Error::Integrity); }
            let bytes: &[u8] = rows.first().ok_or(Error::Integrity)?.try_get(0).map_err(|_| Error::Integrity)?;
            let inbox = MailboxId::from_bytes(bytes.try_into().map_err(|_| Error::Integrity)?);
            Ok(Creation { epoch, account, inbox })
        })
    }).map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}
