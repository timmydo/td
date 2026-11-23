//! Closed replica Email effects; the session owns admission and the transaction.
use crate::postgres_probe::{Client, Row};
use crate::{
    ids::{AccountId, BlobId, MailboxId, ThreadId},
    postgres_configuration::SharedQuotas,
    postgres_probe,
    postgres_publication::{self as publication, Blob},
    postgres_replica_progress,
    postgres_replica_source::Source,
    replica_effect::{Effect, Mail},
};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Source,
    Stale,
    Missing,
    MissingMailbox,
    MissingBody,
    Integrity,
    Capacity,
    Quota,
    Database,
    Indeterminate,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Outcome {
    pub changed: bool,
    pub present: bool,
    pub local_sequence: u64,
}

pub(crate) fn apply(
    client: &Client,
    effect: &Effect,
    deadline: Instant,
    marker: &AtomicBool,
) -> Result<Outcome, Error> {
    let account = effect.account.as_bytes().as_slice();
    let id = effect.id.as_bytes().as_slice();
    let row = client
        .query_opt(
            "SELECT sequence,floor FROM td_mta.accounts WHERE id=$1 FOR UPDATE",
            &[&account],
        )
        .map_err(|_| Error::Database)?
        .ok_or(Error::Missing)?;
    let old = sequence(&row.row(), 0)?;
    if sequence(&row.row(), 1)? > old {
        return Err(Error::Integrity);
    }
    let source =
        Source::new(effect.source.origin(), effect.source.epoch()).map_err(|_| Error::Source)?;
    let mut progress = postgres_replica_progress::read_snapshot(client, source, effect.account)
        .map_err(|_| Error::Integrity)?;
    let pending = progress.pending.as_mut().ok_or(Error::Stale)?;
    pending
        .applied(effect.ticket, effect.id, effect.observed)
        .map_err(|_| Error::Stale)?;
    let identity = effect.mail.as_ref().map(|mail| {
        let mut value = [0u8; 40];
        let (blob, rest) = value.split_at_mut(16);
        let (thread, received) = rest.split_at_mut(16);
        blob.copy_from_slice(mail.blob.as_bytes());
        thread.copy_from_slice(mail.thread.as_bytes());
        received.copy_from_slice(&mail.received_at.to_be_bytes());
        value
    });
    let receipt=client.query_opt("SELECT generation,observed,digest,present,identity FROM td_mta.replica_email_receipts WHERE account=$1 AND id=$2",&[&account, &id]).map_err(|_|Error::Database)?;
    let existing=client.query_opt("SELECT blob_id,thread_id,received_at,changed FROM td_mta.emails WHERE account=$1 AND id=$2",&[&account, &id]).map_err(|_|Error::Database)?;
    let mut equal = false;
    match &receipt {
        Some(row) => {
            let generation = sequence(&row.row(), 0)?;
            let observed = sequence(&row.row(), 1)?;
            let present: bool = row.try_get(3).map_err(|_| Error::Integrity)?;
            if generation > progress.generation || generation == 0 || present != existing.is_some()
            {
                return Err(Error::Integrity);
            }
            if observed > effect.observed.sequence.number() {
                return Err(Error::Stale);
            }
            let saved: Option<&[u8]> = row.try_get(4).map_err(|_| Error::Integrity)?;
            if let (Some(saved), Some(identity)) = (saved, identity) {
                if saved != identity {
                    return Err(Error::Integrity);
                }
            }
            equal = if present {
                bytes::<32>(&row.row(), 2)? == effect.digest
            } else {
                effect.mail.is_none()
            };
            if observed == effect.observed.sequence.number() && !equal {
                return Err(Error::Integrity);
            }
            if equal && present != effect.mail.is_some() {
                return Err(Error::Integrity);
            }
        }
        None => {
            if existing.is_some() {
                return Err(Error::Integrity);
            }
        }
    }
    let mut next = old;
    let changed = !equal && (existing.is_some() || effect.mail.is_some());
    if let Some(row) = &existing {
        if sequence(&row.row(), 3)? > old {
            return Err(Error::Integrity);
        }
    }
    if changed {
        next = old.checked_add(1).ok_or(Error::Capacity)?;
    }
    if let Some(mail) = &effect.mail {
        if let Some(row) = &existing {
            if bytes::<16>(&row.row(), 0)? != *mail.blob.as_bytes()
                || bytes::<16>(&row.row(), 1)? != *mail.thread.as_bytes()
                || row.try_get::<_, i64>(2).map_err(|_| Error::Integrity)? != mail.received_at
            {
                return Err(Error::Integrity);
            }
        }
        if existing.is_none() {
            progress.email_count = charge(progress.email_count, 1, progress.limits.emails)?;
            progress.receipt_count = charge(progress.receipt_count, 1, progress.limits.receipts)?;
        }
        check_mailboxes(client, effect.account, mail, old)?;
        blob(
            client,
            effect,
            mail,
            BlobBudget {
                receipts: &mut progress.receipt_count,
                limits: progress.limits,
                require_existing: existing.is_some(),
            },
            next,
            old,
            deadline,
        )?;
    }
    if changed {
        let old_mailboxes = mailboxes(client, effect.account, effect.id.as_bytes())?;
        let mut affected = old_mailboxes;
        match (&existing, &effect.mail) {
            (Some(_), Some(mail)) => {
                client
                    .execute(
                        "DELETE FROM td_mta.memberships WHERE account=$1 AND email_id=$2",
                        &[&account, &id],
                    )
                    .map_err(|_| Error::Database)?;
                client
                    .execute(
                        "DELETE FROM td_mta.keywords WHERE account=$1 AND email_id=$2",
                        &[&account, &id],
                    )
                    .map_err(|_| Error::Database)?;
                relations(client, effect, mail, next)?;
                if client
                    .execute(
                        "UPDATE td_mta.emails SET changed=$3 WHERE account=$1 AND id=$2",
                        &[&account, &id, &next.to_be_bytes().as_slice()],
                    )
                    .map_err(|_| Error::Database)?
                    != 1
                {
                    return Err(Error::Integrity);
                }
                thread(client, effect.account, mail.thread, next, old, false)?;
                affected.extend_from_slice(&mail.mailboxes);
                history(client, effect.account, next, 0, 3, 2, id)?;
                history(
                    client,
                    effect.account,
                    next,
                    1,
                    2,
                    2,
                    mail.thread.as_bytes(),
                )?;
            }
            (None, Some(mail)) => {
                let created = thread(client, effect.account, mail.thread, next, old, true)?;
                if client.execute("INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,changed) VALUES($1,$2,$3,$4,$5,3,$6)",&[&account, &id, &mail.blob.as_bytes().as_slice(), &mail.thread.as_bytes().as_slice(), &mail.received_at, &next.to_be_bytes().as_slice()]).map_err(|_|Error::Database)?!=1 {return Err(Error::Integrity);}
                relations(client, effect, mail, next)?;
                affected.extend_from_slice(&mail.mailboxes);
                history(client, effect.account, next, 0, 3, 1, id)?;
                history(
                    client,
                    effect.account,
                    next,
                    1,
                    2,
                    if created { 1 } else { 2 },
                    mail.thread.as_bytes(),
                )?;
            }
            (Some(row), None) => {
                let blob = BlobId::from_bytes(bytes(&row.row(), 0)?);
                let thread_id = ThreadId::from_bytes(bytes(&row.row(), 1)?);
                if sequence(&row.row(), 3)? > old {
                    return Err(Error::Integrity);
                }
                progress.email_count = progress
                    .email_count
                    .checked_sub(1)
                    .ok_or(Error::Integrity)?;
                for sql in [
                    "DELETE FROM td_mta.memberships WHERE account=$1 AND email_id=$2",
                    "DELETE FROM td_mta.keywords WHERE account=$1 AND email_id=$2",
                    "DELETE FROM td_mta.thread_anchors WHERE account=$1 AND email_id=$2",
                ] {
                    client
                        .execute(sql, &[&account, &id])
                        .map_err(|_| Error::Database)?;
                }
                if client
                    .execute(
                        "DELETE FROM td_mta.emails WHERE account=$1 AND id=$2",
                        &[&account, &id],
                    )
                    .map_err(|_| Error::Database)?
                    != 1
                {
                    return Err(Error::Integrity);
                }
                let remaining:bool=client.query_one("SELECT EXISTS(SELECT 1 FROM td_mta.emails WHERE account=$1 AND thread_id=$2)",&[&account, &thread_id.as_bytes().as_slice()]).map_err(|_|Error::Database)?.try_get(0).map_err(|_|Error::Integrity)?;
                if remaining {
                    thread(client, effect.account, thread_id, next, old, false)?;
                } else if client
                    .execute(
                        "DELETE FROM td_mta.threads WHERE account=$1 AND id=$2",
                        &[&account, &thread_id.as_bytes().as_slice()],
                    )
                    .map_err(|_| Error::Database)?
                    != 1
                {
                    return Err(Error::Integrity);
                }
                history(client, effect.account, next, 0, 3, 3, id)?;
                history(
                    client,
                    effect.account,
                    next,
                    1,
                    2,
                    if remaining { 2 } else { 3 },
                    thread_id.as_bytes(),
                )?;
                let collected = collect_blob(client, effect.account, blob)?;
                progress.receipt_count = progress
                    .receipt_count
                    .checked_sub(1 + u64::from(collected))
                    .ok_or(Error::Integrity)?;
            }
            (None, None) => return Err(Error::Integrity),
        }
        affected.sort_unstable();
        affected.dedup();
        let mut ids = Vec::new();
        ids.try_reserve_exact(affected.len())
            .map_err(|_| Error::Capacity)?;
        ids.extend(affected.iter().map(|id| id.as_bytes().as_slice()));
        if client
            .execute(
                "UPDATE td_mta.mailboxes SET changed=$3 WHERE account=$1 AND id=ANY($2::bytea[])",
                &[&account, &ids, &next.to_be_bytes().as_slice()],
            )
            .map_err(|_| Error::Database)?
            != ids.len() as u64
        {
            return Err(Error::Integrity);
        }
        if client.execute("INSERT INTO td_mta.changes SELECT $1,$2,ordinality+1,1,2,id FROM unnest($3::bytea[]) WITH ORDINALITY ids(id,ordinality)",&[&account, &next.to_be_bytes().as_slice(), &ids]).map_err(|_|Error::Database)?!=ids.len() as u64{return Err(Error::Integrity);}
        if client
            .execute(
                "UPDATE td_mta.accounts SET sequence=$2 WHERE id=$1",
                &[&account, &next.to_be_bytes().as_slice()],
            )
            .map_err(|_| Error::Database)?
            != 1
        {
            return Err(Error::Integrity);
        }
    }
    let present = effect.mail.is_some();
    let digest = effect.mail.as_ref().map(|_| effect.digest.as_slice());
    let identity = identity.as_ref().map(|value| value.as_slice());
    if client.execute("INSERT INTO td_mta.replica_email_receipts AS r(account,id,generation,observed,digest,present,identity) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(account,id) DO UPDATE SET generation=EXCLUDED.generation,observed=EXCLUDED.observed,digest=EXCLUDED.digest,present=EXCLUDED.present,identity=COALESCE(r.identity,EXCLUDED.identity)",&[&account, &id, &effect.ticket.generation.to_be_bytes().as_slice(), &effect.observed.sequence.number().to_be_bytes().as_slice(), &digest, &present, &identity]).map_err(|_|Error::Database)?!=1 {return Err(Error::Integrity);}
    if client
        .execute(
            "UPDATE td_mta.replica_progress SET email_count=$2,receipt_count=$3 WHERE account=$1",
            &[
                &account,
                &(progress.email_count as i64),
                &(progress.receipt_count as i64),
            ],
        )
        .map_err(|_| Error::Database)?
        != 1
    {
        return Err(Error::Integrity);
    }
    if client.execute("UPDATE td_mta.replica_pending SET done=$4 WHERE account=$1 AND generation=$2 AND page=$3",&[&account, &effect.ticket.generation.to_be_bytes().as_slice(), &effect.ticket.page.to_be_bytes().as_slice(), &pending.done().as_slice()]).map_err(|_|Error::Database)?!=1 {return Err(Error::Stale);}
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    marker.store(true, Ordering::Relaxed);
    client.commit().map_err(|_| Error::Indeterminate)?;
    Ok(Outcome {
        changed,
        present,
        local_sequence: next,
    })
}

fn bytes<const N: usize>(row: &Row, index: usize) -> Result<[u8; N], Error> {
    let value: &[u8] = row.try_get(index).map_err(|_| Error::Integrity)?;
    value.try_into().map_err(|_| Error::Integrity)
}
fn sequence(row: &Row, index: usize) -> Result<u64, Error> {
    Ok(u64::from_be_bytes(bytes(row, index)?))
}
fn charge(used: u64, extra: u64, limit: u64) -> Result<u64, Error> {
    used.checked_add(extra)
        .filter(|n| *n <= limit)
        .ok_or(Error::Quota)
}
fn publication_error(e: publication::Error) -> Error {
    match e {
        publication::Error::Quota => Error::Quota,
        publication::Error::Capacity => Error::Capacity,
        publication::Error::Deadline => Error::Connection(postgres_probe::Error::Deadline),
        publication::Error::Database => Error::Database,
        _ => Error::Integrity,
    }
}
fn mailboxes(client: &Client, account: AccountId, id: &[u8; 16]) -> Result<Vec<MailboxId>, Error> {
    let rows=client.query("SELECT mailbox_id FROM td_mta.memberships WHERE account=$1 AND email_id=$2 ORDER BY mailbox_id LIMIT 257",&[&account.as_bytes().as_slice(), &id.as_slice()]).map_err(|_|Error::Database)?;
    if rows.len() > 256 {
        return Err(Error::Capacity);
    }
    let mut ids = Vec::new();
    ids.try_reserve_exact(rows.len().saturating_add(256))
        .map_err(|_| Error::Capacity)?;
    for row in &rows {
        ids.push(MailboxId::from_bytes(bytes(&row, 0)?));
    }
    Ok(ids)
}
fn check_mailboxes(
    client: &Client,
    account: AccountId,
    mail: &Mail,
    old: u64,
) -> Result<(), Error> {
    if mail.mailboxes.is_empty() || mail.mailboxes.len() > 256 {
        return Err(Error::Capacity);
    }
    let mut ids = Vec::new();
    ids.try_reserve_exact(mail.mailboxes.len())
        .map_err(|_| Error::Capacity)?;
    ids.extend(mail.mailboxes.iter().map(|id| id.as_bytes().as_slice()));
    let row=client.query_one("SELECT count(*),COALESCE(bool_and(m.changed<=$3 AND r.present),false) FROM td_mta.mailboxes m JOIN td_mta.replica_mailbox_receipts r ON r.account=m.account AND r.id=m.id WHERE m.account=$1 AND m.id=ANY($2::bytea[])",&[&account.as_bytes().as_slice(), &ids, &old.to_be_bytes().as_slice()]).map_err(|_|Error::Database)?;
    if row.try_get::<_, i64>(0).map_err(|_| Error::Integrity)? != ids.len() as i64
        || !row.try_get::<_, bool>(1).map_err(|_| Error::Integrity)?
    {
        return Err(Error::MissingMailbox);
    }
    Ok(())
}
fn relations(client: &Client, effect: &Effect, mail: &Mail, next: u64) -> Result<(), Error> {
    let account = effect.account.as_bytes().as_slice();
    let id = effect.id.as_bytes().as_slice();
    let changed = next.to_be_bytes();
    let mut ids = Vec::new();
    ids.try_reserve_exact(mail.mailboxes.len())
        .map_err(|_| Error::Capacity)?;
    ids.extend(mail.mailboxes.iter().map(|id| id.as_bytes().as_slice()));
    if client
        .execute(
            "INSERT INTO td_mta.memberships SELECT $1,$2,id,$4 FROM unnest($3::bytea[]) ids(id)",
            &[&account, &id, &ids, &changed.as_slice()],
        )
        .map_err(|_| Error::Database)?
        != ids.len() as u64
    {
        return Err(Error::Integrity);
    }
    let mut keywords = Vec::new();
    keywords
        .try_reserve_exact(mail.keywords.len())
        .map_err(|_| Error::Capacity)?;
    keywords.extend(mail.keywords.iter().map(String::as_str));
    if client.execute("INSERT INTO td_mta.keywords SELECT $1,$2,keyword,$4 FROM unnest($3::text[]) keys(keyword)",&[&account, &id, &keywords, &changed.as_slice()]).map_err(|_|Error::Database)?!=keywords.len() as u64{return Err(Error::Integrity);}
    Ok(())
}
fn thread(
    client: &Client,
    account: AccountId,
    id: ThreadId,
    next: u64,
    old: u64,
    allow_new: bool,
) -> Result<bool, Error> {
    let row = client
        .query_opt(
            "SELECT changed FROM td_mta.threads WHERE account=$1 AND id=$2",
            &[&account.as_bytes().as_slice(), &id.as_bytes().as_slice()],
        )
        .map_err(|_| Error::Database)?;
    let new = row.is_none();
    if new && !allow_new {
        return Err(Error::Integrity);
    }
    if let Some(row) = row {
        if sequence(&row.row(), 0)? > old {
            return Err(Error::Integrity);
        }
    }
    let sql = if new {
        "INSERT INTO td_mta.threads(account,id,changed) VALUES($1,$2,$3)"
    } else {
        "UPDATE td_mta.threads SET changed=$3 WHERE account=$1 AND id=$2"
    };
    if client
        .execute(
            sql,
            &[
                &account.as_bytes().as_slice(),
                &id.as_bytes().as_slice(),
                &next.to_be_bytes().as_slice(),
            ],
        )
        .map_err(|_| Error::Database)?
        != 1
    {
        return Err(Error::Integrity);
    }
    Ok(new)
}
fn history(
    client: &Client,
    account: AccountId,
    sequence: u64,
    operation: i64,
    kind: i64,
    action: i64,
    id: &[u8],
) -> Result<(), Error> {
    if client.execute("INSERT INTO td_mta.changes(account,sequence,operation,kind,action,object) VALUES($1,$2,$3,$4,$5,$6)",&[&account.as_bytes().as_slice(), &sequence.to_be_bytes().as_slice(), &operation, &kind, &action, &id]).map_err(|_|Error::Database)?!=1{return Err(Error::Integrity);}
    Ok(())
}
struct BlobBudget<'a> {
    receipts: &'a mut u64,
    limits: crate::replica_seed::Limits,
    require_existing: bool,
}
fn blob(
    client: &Client,
    effect: &Effect,
    mail: &Mail,
    budget: BlobBudget<'_>,
    next: u64,
    old: u64,
    deadline: Instant,
) -> Result<(), Error> {
    let BlobBudget {
        receipts,
        limits,
        require_existing,
    } = budget;
    let account = effect.account.as_bytes().as_slice();
    let id = mail.blob.as_bytes().as_slice();
    let length = i64::try_from(mail.length).map_err(|_| Error::Integrity)?;
    let digest = mail.digest;
    let identity = client
        .query_opt(
            "SELECT length,digest FROM td_mta.replica_blob_identities WHERE account=$1 AND id=$2",
            &[&account, &id],
        )
        .map_err(|_| Error::Database)?;
    if let Some(row) = &identity {
        if row.try_get::<_, i64>(0).map_err(|_| Error::Integrity)? != length
            || bytes::<32>(&row.row(), 1)? != digest
        {
            return Err(Error::Integrity);
        }
    }
    let existing = client
        .query_opt(
            "SELECT length,digest,changed FROM td_mta.blobs WHERE account=$1 AND id=$2",
            &[&account, &id],
        )
        .map_err(|_| Error::Database)?;
    if let Some(row) = existing {
        if identity.is_none()
            || row.try_get::<_, i64>(0).map_err(|_| Error::Integrity)? != length
            || bytes::<32>(&row.row(), 1)? != digest
            || sequence(&row.row(), 2)? > old
        {
            return Err(Error::Integrity);
        }
        return Ok(());
    }
    if require_existing {
        return Err(Error::Integrity);
    }
    let body = mail.body.as_ref().ok_or(Error::MissingBody)?;
    let caps = SharedQuotas {
        body_bytes: limits.body_bytes,
        blob_count: limits.blobs,
        upload_bytes: 0,
        queue_bytes: 0,
        queue_submissions: 0,
    };
    let usage = publication::charge(client, effect.account, caps, [length as u64, 1, 0, 0, 0])
        .map_err(publication_error)?;
    let created_at: i64 = client
        .query_one(
            "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
            &[],
        )
        .map_err(|_| Error::Database)?
        .try_get(0)
        .map_err(|_| Error::Integrity)?;
    let value = Blob {
        account: effect.account,
        id: mail.blob,
        bytes: body.bytes(),
        digest: &digest,
        interpretation: mail.interpretation.as_ref(),
        created_at,
        changed: next,
    };
    *receipts = charge(*receipts, 1, limits.receipts)?;
    if identity.is_some() {
        publication::write_reserved_blob(client, value, deadline).map_err(publication_error)?;
    } else {
        publication::write_blob(client, value, deadline).map_err(publication_error)?;
        if client.execute("INSERT INTO td_mta.replica_blob_identities(account,id,length,digest) VALUES($1,$2,$3,$4)",&[&account, &id, &length, &digest.as_slice()]).map_err(|_|Error::Database)?!=1{return Err(Error::Integrity);}
    }
    usage
        .publish(client, effect.account)
        .map_err(publication_error)
}
fn collect_blob(client: &Client, account_id: AccountId, blob: BlobId) -> Result<bool, Error> {
    let account = account_id.as_bytes().as_slice();
    let id = blob.as_bytes().as_slice();
    let deleted=client.query_opt("DELETE FROM td_mta.blobs b WHERE b.account=$1 AND b.id=$2 AND NOT EXISTS(SELECT 1 FROM td_mta.emails e WHERE e.account=b.account AND e.blob_id=b.id) AND NOT EXISTS(SELECT 1 FROM td_mta.leases l WHERE l.account=b.account AND l.blob_id=b.id) AND NOT EXISTS(SELECT 1 FROM td_mta.submissions s WHERE s.account=b.account AND s.transmitted_blob_id=b.id) RETURNING length",&[&account, &id]).map_err(|_|Error::Database)?;
    if let Some(row) = &deleted {
        let length: i64 = row.try_get(0).map_err(|_| Error::Integrity)?;
        if length < 0 {
            return Err(Error::Integrity);
        }
        if client.execute("UPDATE td_mta.account_usage SET body_bytes=body_bytes-$2,blob_count=blob_count-1 WHERE account=$1 AND body_bytes>=$2 AND blob_count>=1",&[&account, &length]).map_err(|_|Error::Database)?!=1{return Err(Error::Integrity);}
    }
    Ok(deleted.is_some())
}

/// A finite, thread-owned copy connection with closed replica operations.
pub struct Publisher {
    session: postgres_probe::ReplicaSession,
}
impl Publisher {
    pub fn open(
        request: &postgres_probe::Request<'_>,
        deadline: Instant,
        epoch: crate::ids::StoreEpoch,
        source: Source,
        account: AccountId,
    ) -> Result<Self, Error> {
        Ok(Self {
            session: postgres_probe::ReplicaSession::open(
                request, deadline, epoch, source, account,
            )
            .map_err(Error::Connection)?,
        })
    }
    /// Publish a complete graph while retaining referenced retired folders.
    pub fn mailboxes(
        &mut self,
        graph: std::sync::Arc<crate::replica_mailboxes::Graph>,
        deadline: Instant,
    ) -> Result<crate::postgres_replica_mailboxes::Outcome, Error> {
        self.session.mailboxes(graph, deadline)
    }
    /// A consistent historical observation; it grants no future write authority.
    pub fn inspect(
        &mut self,
        deadline: Instant,
    ) -> Result<crate::postgres_replica_progress::Progress, Error> {
        self.session.inspect(deadline)
    }
    pub fn advance(
        &mut self,
        transition: std::sync::Arc<crate::replica_transition::Transition>,
        deadline: Instant,
    ) -> Result<crate::replica_pending::Ticket, Error> {
        self.session.advance(transition, deadline)
    }
    pub fn reusable(&self, deadline: Instant) -> bool {
        self.session.reusable(deadline)
    }
    pub fn publish(
        &mut self,
        effect: std::sync::Arc<Effect>,
        deadline: Instant,
    ) -> Result<Outcome, Error> {
        self.session.publish(effect, deadline)
    }
}
