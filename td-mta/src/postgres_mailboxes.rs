//! Finite authenticated mailbox observations; no runtime or configuration admission.
use crate::{
    device_credentials::Presented,
    format::{
        key::Key,
        row::{MailboxRow, Row},
        Sequence,
    },
    ids::{AccountId, DeviceId, MailboxId, StoreEpoch},
    ports::ViewIdentity,
    postgres_devices,
    postgres_probe::{self, Request},
    postgres_schema,
};
use std::time::Instant;
use td_crypto::TrustStore;

pub const MAX_MAILBOXES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Credential(postgres_devices::Error),
    Epoch,
    Unauthenticated,
    Missing,
    Capacity,
    Integrity,
    Database,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Counts {
    pub total_emails: u64,
    pub unread_emails: u64,
    pub total_threads: u64,
    pub unread_threads: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Mailbox {
    pub id: MailboxId,
    pub name: String,
    pub parent: Option<MailboxId>,
    pub role: Option<String>,
    pub sort_order: u32,
    pub subscribed: bool,
    pub changed: u64,
    pub counts: Counts,
}
impl Mailbox {
    pub fn row(&self) -> MailboxRow<'_> {
        MailboxRow {
            name: &self.name,
            parent: self.parent,
            role: self.role.as_deref(),
            sort_order: self.sort_order,
            subscribed: self.subscribed,
        }
    }
}

/// Detached results from one closed snapshot. Neither the device ID nor this
/// state authorizes any subsequent database operation.
#[derive(Debug, Eq, PartialEq)]
pub struct Listing {
    pub identity: ViewIdentity,
    pub device: DeviceId,
    pub mailboxes: Vec<Mailbox>,
}

// LIMIT bounds driver allocation even when the account exceeds local capacity.
// Counts exclude drafts from unread totals, exactly as the existing mail view.
const MAILBOXES: &str = "WITH selected AS MATERIALIZED (
    SELECT id,name,parent_id,role,sort_order,subscribed,changed
    FROM td_mta.mailboxes WHERE account=$1 ORDER BY id LIMIT 257
) SELECT b.id,b.name,b.parent_id,b.role,b.sort_order,b.subscribed,b.changed,
    c.total,c.unread,c.threads,c.unread_threads,c.valid
FROM selected b CROSS JOIN LATERAL (
    SELECT count(*) AS total,
        count(*) FILTER (WHERE s.email_id IS NULL AND d.email_id IS NULL) AS unread,
        count(DISTINCT e.thread_id) AS threads,
        count(DISTINCT e.thread_id) FILTER (WHERE s.email_id IS NULL AND d.email_id IS NULL) AS unread_threads,
        coalesce(bool_and(e.id IS NOT NULL AND t.id IS NOT NULL
            AND e.changed<=$2 AND t.changed<=$2 AND m.changed<=$2
            AND (s.email_id IS NULL OR s.changed<=$2)
            AND (d.email_id IS NULL OR d.changed<=$2)),true) AS valid
    FROM td_mta.memberships m
    LEFT JOIN td_mta.emails e ON e.account=m.account AND e.id=m.email_id
    LEFT JOIN td_mta.threads t ON t.account=e.account AND t.id=e.thread_id
    LEFT JOIN td_mta.keywords s ON s.account=e.account AND s.email_id=e.id AND s.keyword='$seen'
    LEFT JOIN td_mta.keywords d ON d.account=e.account AND d.email_id=e.id AND d.keyword='$draft'
    WHERE m.account=$1 AND m.mailbox_id=b.id
) c ORDER BY b.id";

/// Authenticate and read metadata/counts/state in one primary repeatable-read
/// snapshot. Return nothing on overflow, corruption, authentication failure or
/// expiry. Callers must separately admit the selected configuration and role.
pub fn list(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    account: AccountId,
    password: &[u8],
) -> Result<Listing, Error> {
    let presented = Presented::parse(password).ok_or(Error::Unauthenticated)?;
    let result = postgres_probe::with_mailbox_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let schema = postgres_schema::lock_and_inspect(client).await.map_err(Error::Schema)?;
            if schema.store_epoch != epoch { return Err(Error::Epoch); }
            client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000").await.map_err(|_| Error::Database)?;
            let device = postgres_devices::authenticate(client, account, presented).await.map_err(Error::Credential)?.ok_or(Error::Unauthenticated)?;
            let rows = client.query("SELECT sequence,floor FROM td_mta.accounts WHERE id=$1 LIMIT 2", &[&account.as_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
            if rows.is_empty() { return Err(Error::Missing); }
            if rows.len() != 1 { return Err(Error::Integrity); }
            let row = rows.first().ok_or(Error::Integrity)?;
            let sequence = sequence(row.try_get(0).map_err(|_| Error::Integrity)?)?;
            let floor = sequence_from_row(row, 1)?;
            if floor > sequence { return Err(Error::Integrity); }
            let identity = ViewIdentity { account, epoch, committed_sequence: Sequence::from_u64(sequence), history_floor: Sequence::from_u64(floor) };
            let rows = client.query(MAILBOXES, &[&account.as_bytes().as_slice(), &sequence.to_be_bytes().as_slice()]).await.map_err(|_| Error::Database)?;
            if rows.len() > MAX_MAILBOXES { return Err(Error::Capacity); }
            let mut mailboxes = Vec::new();
            mailboxes.try_reserve_exact(rows.len()).map_err(|_| Error::Capacity)?;
            for row in &rows {
                if Instant::now() >= deadline { return Err(Error::Connection(postgres_probe::Error::Deadline)); }
                mailboxes.push(decode(row, sequence)?);
            }
            hierarchy(&mailboxes)?;
            Ok(Listing { identity, device, mailboxes })
        })
    }).map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}

fn sequence(bytes: &[u8]) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| Error::Integrity)?,
    ))
}
fn sequence_from_row(row: &tokio_postgres::Row, column: usize) -> Result<u64, Error> {
    sequence(row.try_get(column).map_err(|_| Error::Integrity)?)
}
fn text(value: &str) -> Result<String, Error> {
    let mut copy = String::new();
    copy.try_reserve_exact(value.len())
        .map_err(|_| Error::Capacity)?;
    copy.push_str(value);
    Ok(copy)
}
fn decode(row: &tokio_postgres::Row, sequence: u64) -> Result<Mailbox, Error> {
    let id: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
    let id = MailboxId::from_bytes(id.try_into().map_err(|_| Error::Integrity)?);
    let name: &str = row.try_get(1).map_err(|_| Error::Integrity)?;
    let parent: Option<&[u8]> = row.try_get(2).map_err(|_| Error::Integrity)?;
    let parent = parent
        .map(|bytes| {
            bytes
                .try_into()
                .map(MailboxId::from_bytes)
                .map_err(|_| Error::Integrity)
        })
        .transpose()?;
    let role: Option<&str> = row.try_get(3).map_err(|_| Error::Integrity)?;
    let order: i64 = row.try_get(4).map_err(|_| Error::Integrity)?;
    let subscribed: i64 = row.try_get(5).map_err(|_| Error::Integrity)?;
    if !matches!(subscribed, 0 | 1) {
        return Err(Error::Integrity);
    }
    let borrowed = MailboxRow {
        name,
        parent,
        role,
        sort_order: u32::try_from(order).map_err(|_| Error::Integrity)?,
        subscribed: subscribed == 1,
    };
    Row::Mailbox(borrowed)
        .validate_key(Key::Mailbox(id))
        .map_err(|_| Error::Integrity)?;
    let changed = sequence_from_row(row, 6)?;
    let valid: bool = row.try_get(11).map_err(|_| Error::Integrity)?;
    if changed > sequence || !valid {
        return Err(Error::Integrity);
    }
    let count = |column| -> Result<u64, Error> {
        let value: i64 = row.try_get(column).map_err(|_| Error::Integrity)?;
        u64::try_from(value).map_err(|_| Error::Integrity)
    };
    let counts = Counts {
        total_emails: count(7usize)?,
        unread_emails: count(8usize)?,
        total_threads: count(9usize)?,
        unread_threads: count(10usize)?,
    };
    if counts.unread_emails > counts.total_emails
        || counts.total_threads > counts.total_emails
        || counts.unread_threads > counts.total_threads
        || counts.unread_threads > counts.unread_emails
    {
        return Err(Error::Integrity);
    }
    Ok(Mailbox {
        id,
        name: text(name)?,
        parent,
        role: role.map(text).transpose()?,
        sort_order: borrowed.sort_order,
        subscribed: borrowed.subscribed,
        changed,
        counts,
    })
}

fn hierarchy(mailboxes: &[Mailbox]) -> Result<(), Error> {
    if mailboxes
        .windows(2)
        .any(|pair| pair.first().map(|row| row.id) >= pair.last().map(|row| row.id))
    {
        return Err(Error::Integrity);
    }
    for mailbox in mailboxes {
        let mut parent = mailbox.parent;
        let mut remaining = mailboxes.len();
        while let Some(id) = parent {
            remaining = remaining.checked_sub(1).ok_or(Error::Integrity)?;
            let index = mailboxes
                .binary_search_by_key(&id, |row| row.id)
                .map_err(|_| Error::Integrity)?;
            parent = mailboxes.get(index).ok_or(Error::Integrity)?.parent;
        }
    }
    Ok(())
}
