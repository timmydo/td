//! Closed replica Email effects; the session owns admission and the transaction.
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
use tokio_postgres::{types::Type, Client, Row};

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

pub(crate) async fn apply(
    client: &Client,
    effect: &Effect,
    deadline: Instant,
    marker: &AtomicBool,
) -> Result<Outcome, Error> {
    let account = effect.account.as_bytes().as_slice();
    let id = effect.id.as_bytes().as_slice();
    let row = client
        .query_typed_opt(
            "SELECT sequence,floor FROM td_mta.accounts WHERE id=$1 FOR UPDATE",
            &[(&account, Type::BYTEA)],
        )
        .await
        .map_err(|_| Error::Database)?
        .ok_or(Error::Missing)?;
    let old = sequence(&row, 0)?;
    if sequence(&row, 1)? > old {
        return Err(Error::Integrity);
    }
    let source =
        Source::new(effect.source.origin(), effect.source.epoch()).map_err(|_| Error::Source)?;
    let mut progress = postgres_replica_progress::read_snapshot(client, source, effect.account)
        .await
        .map_err(|_| Error::Integrity)?;
    let pending = progress.pending.as_mut().ok_or(Error::Stale)?;
    pending
        .applied(effect.ticket, effect.id, effect.observed)
        .map_err(|_| Error::Stale)?;
    let receipt=client.query_typed_opt("SELECT generation,observed,digest,present FROM td_mta.replica_email_receipts WHERE account=$1 AND id=$2",&[(&account,Type::BYTEA),(&id,Type::BYTEA)]).await.map_err(|_|Error::Database)?;
    let existing=client.query_typed_opt("SELECT blob_id,thread_id,received_at,changed FROM td_mta.emails WHERE account=$1 AND id=$2",&[(&account,Type::BYTEA),(&id,Type::BYTEA)]).await.map_err(|_|Error::Database)?;
    let mut equal = false;
    match &receipt {
        Some(row) => {
            let generation = sequence(row, 0)?;
            let observed = sequence(row, 1)?;
            let digest: [u8; 32] = bytes(row, 2)?;
            let present: bool = row.try_get(3).map_err(|_| Error::Integrity)?;
            if generation > progress.generation || generation == 0 || present != existing.is_some()
            {
                return Err(Error::Integrity);
            }
            if observed > effect.observed.sequence.number() {
                return Err(Error::Stale);
            }
            equal = digest == effect.digest;
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
            progress.receipt_count = charge(progress.receipt_count, 1, progress.limits.receipts)?;
        }
    }
    let mut next = old;
    let changed = !equal && (existing.is_some() || effect.mail.is_some());
    if let Some(row) = &existing {
        if sequence(row, 3)? > old {
            return Err(Error::Integrity);
        }
    }
    if changed {
        next = old.checked_add(1).ok_or(Error::Capacity)?;
    }
    if let Some(mail) = &effect.mail {
        if let Some(row) = &existing {
            if bytes::<16>(row, 0)? != *mail.blob.as_bytes()
                || bytes::<16>(row, 1)? != *mail.thread.as_bytes()
                || row.try_get::<_, i64>(2).map_err(|_| Error::Integrity)? != mail.received_at
            {
                return Err(Error::Integrity);
            }
        }
        check_mailboxes(client, effect.account, mail, old).await?;
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
        )
        .await?;
    }
    if changed {
        let old_mailboxes = mailboxes(client, effect.account, effect.id.as_bytes()).await?;
        let mut affected = old_mailboxes;
        match (&existing, &effect.mail) {
            (Some(_), Some(mail)) => {
                client
                    .execute_typed(
                        "DELETE FROM td_mta.memberships WHERE account=$1 AND email_id=$2",
                        &[(&account, Type::BYTEA), (&id, Type::BYTEA)],
                    )
                    .await
                    .map_err(|_| Error::Database)?;
                client
                    .execute_typed(
                        "DELETE FROM td_mta.keywords WHERE account=$1 AND email_id=$2",
                        &[(&account, Type::BYTEA), (&id, Type::BYTEA)],
                    )
                    .await
                    .map_err(|_| Error::Database)?;
                relations(client, effect, mail, next).await?;
                if client
                    .execute_typed(
                        "UPDATE td_mta.emails SET changed=$3 WHERE account=$1 AND id=$2",
                        &[
                            (&account, Type::BYTEA),
                            (&id, Type::BYTEA),
                            (&next.to_be_bytes().as_slice(), Type::BYTEA),
                        ],
                    )
                    .await
                    .map_err(|_| Error::Database)?
                    != 1
                {
                    return Err(Error::Integrity);
                }
                thread(client, effect.account, mail.thread, next, old, false).await?;
                affected.extend_from_slice(&mail.mailboxes);
                history(client, effect.account, next, 0, 3, 2, id).await?;
                history(
                    client,
                    effect.account,
                    next,
                    1,
                    2,
                    2,
                    mail.thread.as_bytes(),
                )
                .await?;
            }
            (None, Some(mail)) => {
                progress.email_count = charge(progress.email_count, 1, progress.limits.emails)?;
                let created = thread(client, effect.account, mail.thread, next, old, true).await?;
                if client.execute_typed("INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,changed) VALUES($1,$2,$3,$4,$5,3,$6)",&[(&account,Type::BYTEA),(&id,Type::BYTEA),(&mail.blob.as_bytes().as_slice(),Type::BYTEA),(&mail.thread.as_bytes().as_slice(),Type::BYTEA),(&mail.received_at,Type::INT8),(&next.to_be_bytes().as_slice(),Type::BYTEA)]).await.map_err(|_|Error::Database)?!=1 {return Err(Error::Integrity);}
                relations(client, effect, mail, next).await?;
                affected.extend_from_slice(&mail.mailboxes);
                history(client, effect.account, next, 0, 3, 1, id).await?;
                history(
                    client,
                    effect.account,
                    next,
                    1,
                    2,
                    if created { 1 } else { 2 },
                    mail.thread.as_bytes(),
                )
                .await?;
            }
            (Some(row), None) => {
                let blob = BlobId::from_bytes(bytes(row, 0)?);
                let thread_id = ThreadId::from_bytes(bytes(row, 1)?);
                if sequence(row, 3)? > old {
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
                        .execute_typed(sql, &[(&account, Type::BYTEA), (&id, Type::BYTEA)])
                        .await
                        .map_err(|_| Error::Database)?;
                }
                if client
                    .execute_typed(
                        "DELETE FROM td_mta.emails WHERE account=$1 AND id=$2",
                        &[(&account, Type::BYTEA), (&id, Type::BYTEA)],
                    )
                    .await
                    .map_err(|_| Error::Database)?
                    != 1
                {
                    return Err(Error::Integrity);
                }
                let remaining:bool=client.query_typed_one("SELECT EXISTS(SELECT 1 FROM td_mta.emails WHERE account=$1 AND thread_id=$2)",&[(&account,Type::BYTEA),(&thread_id.as_bytes().as_slice(),Type::BYTEA)]).await.map_err(|_|Error::Database)?.try_get(0).map_err(|_|Error::Integrity)?;
                if remaining {
                    thread(client, effect.account, thread_id, next, old, false).await?;
                } else if client
                    .execute_typed(
                        "DELETE FROM td_mta.threads WHERE account=$1 AND id=$2",
                        &[
                            (&account, Type::BYTEA),
                            (&thread_id.as_bytes().as_slice(), Type::BYTEA),
                        ],
                    )
                    .await
                    .map_err(|_| Error::Database)?
                    != 1
                {
                    return Err(Error::Integrity);
                }
                history(client, effect.account, next, 0, 3, 3, id).await?;
                history(
                    client,
                    effect.account,
                    next,
                    1,
                    2,
                    if remaining { 2 } else { 3 },
                    thread_id.as_bytes(),
                )
                .await?;
                collect_blob(client, effect.account, blob).await?;
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
            .execute_typed(
                "UPDATE td_mta.mailboxes SET changed=$3 WHERE account=$1 AND id=ANY($2::bytea[])",
                &[
                    (&account, Type::BYTEA),
                    (&ids, Type::BYTEA_ARRAY),
                    (&next.to_be_bytes().as_slice(), Type::BYTEA),
                ],
            )
            .await
            .map_err(|_| Error::Database)?
            != ids.len() as u64
        {
            return Err(Error::Integrity);
        }
        if client.execute_typed("INSERT INTO td_mta.changes SELECT $1,$2,ordinality+1,1,2,id FROM unnest($3::bytea[]) WITH ORDINALITY ids(id,ordinality)",&[(&account,Type::BYTEA),(&next.to_be_bytes().as_slice(),Type::BYTEA),(&ids,Type::BYTEA_ARRAY)]).await.map_err(|_|Error::Database)?!=ids.len() as u64{return Err(Error::Integrity);}
        if client
            .execute_typed(
                "UPDATE td_mta.accounts SET sequence=$2 WHERE id=$1",
                &[
                    (&account, Type::BYTEA),
                    (&next.to_be_bytes().as_slice(), Type::BYTEA),
                ],
            )
            .await
            .map_err(|_| Error::Database)?
            != 1
        {
            return Err(Error::Integrity);
        }
    }
    let present = effect.mail.is_some();
    if client.execute_typed("INSERT INTO td_mta.replica_email_receipts(account,id,generation,observed,digest,present) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(account,id) DO UPDATE SET generation=EXCLUDED.generation,observed=EXCLUDED.observed,digest=EXCLUDED.digest,present=EXCLUDED.present",&[(&account,Type::BYTEA),(&id,Type::BYTEA),(&effect.ticket.generation.to_be_bytes().as_slice(),Type::BYTEA),(&effect.observed.sequence.number().to_be_bytes().as_slice(),Type::BYTEA),(&effect.digest.as_slice(),Type::BYTEA),(&present,Type::BOOL)]).await.map_err(|_|Error::Database)?!=1 {return Err(Error::Integrity);}
    if client
        .execute_typed(
            "UPDATE td_mta.replica_progress SET email_count=$2,receipt_count=$3 WHERE account=$1",
            &[
                (&account, Type::BYTEA),
                (&(progress.email_count as i64), Type::INT8),
                (&(progress.receipt_count as i64), Type::INT8),
            ],
        )
        .await
        .map_err(|_| Error::Database)?
        != 1
    {
        return Err(Error::Integrity);
    }
    if client.execute_typed("UPDATE td_mta.replica_pending SET done=$4 WHERE account=$1 AND generation=$2 AND page=$3",&[(&account,Type::BYTEA),(&effect.ticket.generation.to_be_bytes().as_slice(),Type::BYTEA),(&effect.ticket.page.to_be_bytes().as_slice(),Type::BYTEA),(&pending.done().as_slice(),Type::BYTEA)]).await.map_err(|_|Error::Database)?!=1 {return Err(Error::Stale);}
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    marker.store(true, Ordering::Relaxed);
    client
        .batch_execute("COMMIT")
        .await
        .map_err(|_| Error::Indeterminate)?;
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
async fn mailboxes(
    client: &Client,
    account: AccountId,
    id: &[u8; 16],
) -> Result<Vec<MailboxId>, Error> {
    let rows=client.query_typed("SELECT mailbox_id FROM td_mta.memberships WHERE account=$1 AND email_id=$2 ORDER BY mailbox_id LIMIT 257",&[(&account.as_bytes().as_slice(),Type::BYTEA),(&id.as_slice(),Type::BYTEA)]).await.map_err(|_|Error::Database)?;
    if rows.len() > 256 {
        return Err(Error::Capacity);
    }
    let mut ids = Vec::new();
    ids.try_reserve_exact(rows.len().saturating_add(256))
        .map_err(|_| Error::Capacity)?;
    for row in rows {
        ids.push(MailboxId::from_bytes(bytes(&row, 0)?));
    }
    Ok(ids)
}
async fn check_mailboxes(
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
    let row=client.query_typed_one("SELECT count(*),COALESCE(bool_and(m.changed<=$3 AND r.present),false) FROM td_mta.mailboxes m JOIN td_mta.replica_mailbox_receipts r ON r.account=m.account AND r.id=m.id WHERE m.account=$1 AND m.id=ANY($2::bytea[])",&[(&account.as_bytes().as_slice(),Type::BYTEA),(&ids,Type::BYTEA_ARRAY),(&old.to_be_bytes().as_slice(),Type::BYTEA)]).await.map_err(|_|Error::Database)?;
    if row.try_get::<_, i64>(0).map_err(|_| Error::Integrity)? != ids.len() as i64
        || !row.try_get::<_, bool>(1).map_err(|_| Error::Integrity)?
    {
        return Err(Error::MissingMailbox);
    }
    Ok(())
}
async fn relations(client: &Client, effect: &Effect, mail: &Mail, next: u64) -> Result<(), Error> {
    let account = effect.account.as_bytes().as_slice();
    let id = effect.id.as_bytes().as_slice();
    let changed = next.to_be_bytes();
    let mut ids = Vec::new();
    ids.try_reserve_exact(mail.mailboxes.len())
        .map_err(|_| Error::Capacity)?;
    ids.extend(mail.mailboxes.iter().map(|id| id.as_bytes().as_slice()));
    if client
        .execute_typed(
            "INSERT INTO td_mta.memberships SELECT $1,$2,id,$4 FROM unnest($3::bytea[]) ids(id)",
            &[
                (&account, Type::BYTEA),
                (&id, Type::BYTEA),
                (&ids, Type::BYTEA_ARRAY),
                (&changed.as_slice(), Type::BYTEA),
            ],
        )
        .await
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
    if client.execute_typed("INSERT INTO td_mta.keywords SELECT $1,$2,keyword,$4 FROM unnest($3::text[]) keys(keyword)",&[(&account,Type::BYTEA),(&id,Type::BYTEA),(&keywords,Type::TEXT_ARRAY),(&changed.as_slice(),Type::BYTEA)]).await.map_err(|_|Error::Database)?!=keywords.len() as u64{return Err(Error::Integrity);}
    Ok(())
}
async fn thread(
    client: &Client,
    account: AccountId,
    id: ThreadId,
    next: u64,
    old: u64,
    allow_new: bool,
) -> Result<bool, Error> {
    let row = client
        .query_typed_opt(
            "SELECT changed FROM td_mta.threads WHERE account=$1 AND id=$2",
            &[
                (&account.as_bytes().as_slice(), Type::BYTEA),
                (&id.as_bytes().as_slice(), Type::BYTEA),
            ],
        )
        .await
        .map_err(|_| Error::Database)?;
    let new = row.is_none();
    if new && !allow_new {
        return Err(Error::Integrity);
    }
    if let Some(row) = row {
        if sequence(&row, 0)? > old {
            return Err(Error::Integrity);
        }
    }
    let sql = if new {
        "INSERT INTO td_mta.threads(account,id,changed) VALUES($1,$2,$3)"
    } else {
        "UPDATE td_mta.threads SET changed=$3 WHERE account=$1 AND id=$2"
    };
    if client
        .execute_typed(
            sql,
            &[
                (&account.as_bytes().as_slice(), Type::BYTEA),
                (&id.as_bytes().as_slice(), Type::BYTEA),
                (&next.to_be_bytes().as_slice(), Type::BYTEA),
            ],
        )
        .await
        .map_err(|_| Error::Database)?
        != 1
    {
        return Err(Error::Integrity);
    }
    Ok(new)
}
async fn history(
    client: &Client,
    account: AccountId,
    sequence: u64,
    operation: i64,
    kind: i64,
    action: i64,
    id: &[u8],
) -> Result<(), Error> {
    if client.execute_typed("INSERT INTO td_mta.changes(account,sequence,operation,kind,action,object) VALUES($1,$2,$3,$4,$5,$6)",&[(&account.as_bytes().as_slice(),Type::BYTEA),(&sequence.to_be_bytes().as_slice(),Type::BYTEA),(&operation,Type::INT8),(&kind,Type::INT8),(&action,Type::INT8),(&id,Type::BYTEA)]).await.map_err(|_|Error::Database)?!=1{return Err(Error::Integrity);}
    Ok(())
}
struct BlobBudget<'a> {
    receipts: &'a mut u64,
    limits: crate::replica_seed::Limits,
    require_existing: bool,
}
async fn blob(
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
        .query_typed_opt(
            "SELECT length,digest FROM td_mta.replica_blob_identities WHERE account=$1 AND id=$2",
            &[(&account, Type::BYTEA), (&id, Type::BYTEA)],
        )
        .await
        .map_err(|_| Error::Database)?;
    if let Some(row) = &identity {
        if row.try_get::<_, i64>(0).map_err(|_| Error::Integrity)? != length
            || bytes::<32>(row, 1)? != digest
        {
            return Err(Error::Integrity);
        }
    }
    let existing = client
        .query_typed_opt(
            "SELECT length,digest,changed FROM td_mta.blobs WHERE account=$1 AND id=$2",
            &[(&account, Type::BYTEA), (&id, Type::BYTEA)],
        )
        .await
        .map_err(|_| Error::Database)?;
    if let Some(row) = existing {
        if identity.is_none()
            || row.try_get::<_, i64>(0).map_err(|_| Error::Integrity)? != length
            || bytes::<32>(&row, 1)? != digest
            || sequence(&row, 2)? > old
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
        .await
        .map_err(publication_error)?;
    let created_at: i64 = client
        .query_typed_one(
            "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
            &[],
        )
        .await
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
    if identity.is_some() {
        publication::write_reserved_blob(client, value, deadline)
            .await
            .map_err(publication_error)?;
    } else {
        *receipts = charge(*receipts, 1, limits.receipts)?;
        publication::write_blob(client, value, deadline)
            .await
            .map_err(publication_error)?;
        if client.execute_typed("INSERT INTO td_mta.replica_blob_identities(account,id,length,digest) VALUES($1,$2,$3,$4)",&[(&account,Type::BYTEA),(&id,Type::BYTEA),(&length,Type::INT8),(&digest.as_slice(),Type::BYTEA)]).await.map_err(|_|Error::Database)?!=1{return Err(Error::Integrity);}
    }
    usage
        .publish(client, effect.account)
        .await
        .map_err(publication_error)
}
async fn collect_blob(client: &Client, account_id: AccountId, blob: BlobId) -> Result<(), Error> {
    let account = account_id.as_bytes().as_slice();
    let id = blob.as_bytes().as_slice();
    let deleted=client.query_typed_opt("DELETE FROM td_mta.blobs b WHERE b.account=$1 AND b.id=$2 AND NOT EXISTS(SELECT 1 FROM td_mta.emails e WHERE e.account=b.account AND e.blob_id=b.id) AND NOT EXISTS(SELECT 1 FROM td_mta.leases l WHERE l.account=b.account AND l.blob_id=b.id) AND NOT EXISTS(SELECT 1 FROM td_mta.submissions s WHERE s.account=b.account AND s.transmitted_blob_id=b.id) RETURNING length",&[(&account,Type::BYTEA),(&id,Type::BYTEA)]).await.map_err(|_|Error::Database)?;
    if let Some(row) = deleted {
        let length: i64 = row.try_get(0).map_err(|_| Error::Integrity)?;
        if length < 0 {
            return Err(Error::Integrity);
        }
        if client.execute_typed("UPDATE td_mta.account_usage SET body_bytes=body_bytes-$2,blob_count=blob_count-1 WHERE account=$1 AND body_bytes>=$2 AND blob_count>=1",&[(&account,Type::BYTEA),(&length,Type::INT8)]).await.map_err(|_|Error::Database)?!=1{return Err(Error::Integrity);}
    }
    Ok(())
}

/// A finite, thread-owned copy connection with only the closed publish operation.
pub struct Publisher {
    session: postgres_probe::ReplicaSession,
}
impl Publisher {
    pub fn open(
        request: &postgres_probe::Request<'_>,
        trust: &td_crypto::TrustStore,
        deadline: Instant,
        epoch: crate::ids::StoreEpoch,
        source: Source,
        account: AccountId,
    ) -> Result<Self, Error> {
        Ok(Self {
            session: postgres_probe::ReplicaSession::open(
                request, trust, deadline, epoch, source, account,
            )
            .map_err(Error::Connection)?,
        })
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
