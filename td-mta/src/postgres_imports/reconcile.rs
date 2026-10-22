//! Owner-selected source metadata reconciliation, with retained receipt fences.
use super::*;
use crate::{
    format::key::SourceKind,
    import_snapshot::{Email, EmailInput},
};
use std::sync::{atomic::AtomicBool, Arc};

#[derive(Clone)]
pub struct Baseline {
    pub epoch: StoreEpoch,
    pub account: AccountId,
    pub key: Key,
    pub local: [u8; 16],
    pub historical_blob: [u8; 16],
    pub folders: Vec<FolderFence>,
    pub digest: [u8; 32],
    pub changed: u64,
    pub desired: [u8; 32],
}
pub enum Plan {
    New,
    Current,
    Absent,
    LocalChanged,
    ImmutableChanged,
    FolderChanged,
    Update(Box<Baseline>),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Applied {
    Updated,
    CurrentReceipt,
    Absent,
}
struct Receipt {
    local: [u8; 16],
    blob: [u8; 16],
    digest: [u8; 32],
    changed: u64,
}
async fn receipt(
    client: &Client,
    authority: &Authority,
    key: &Key,
    old: u64,
) -> Result<Option<Receipt>, Error> {
    if key.kind != SourceKind::Email {
        return Err(Error::Invalid);
    }
    let row=client.query_opt("SELECT local_object,historical_blob,source_digest,changed FROM td_mta.imports WHERE account=$1 AND source_instance=$2 AND source_kind=3 AND source_account=$3 AND source_object=$4", &[&authority.account.as_bytes().as_slice(),&key.instance.as_bytes().as_slice(),&key.account.as_slice(),&key.object.as_slice()]).await.map_err(|_|Error::Database)?;
    let Some(row) = row else { return Ok(None) };
    let changed = sequence(row.try_get(3).map_err(|_| Error::Integrity)?)?;
    if changed > old {
        return Err(Error::Integrity);
    }
    Ok(Some(Receipt {
        local: fixed(row.try_get(0).map_err(|_| Error::Integrity)?)?,
        blob: fixed(row.try_get(1).map_err(|_| Error::Integrity)?)?,
        digest: fixed(row.try_get(2).map_err(|_| Error::Integrity)?)?,
        changed,
    }))
}
async fn local(
    client: &Client,
    authority: &Authority,
    key: &Key,
    receipt: &Receipt,
    old: u64,
) -> Result<Option<Email>, Error> {
    let account = authority.account.as_bytes().as_slice();
    let row=client.query_opt("SELECT e.blob_id,e.received_at,e.origin,e.changed,b.length,b.digest,b.changed FROM td_mta.emails e LEFT JOIN td_mta.blobs b ON b.account=e.account AND b.id=e.blob_id WHERE e.account=$1 AND e.id=$2",&[&account,&receipt.local.as_slice()]).await.map_err(|_|Error::Database)?;
    let Some(row) = row else { return Ok(None) };
    let blob: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
    let origin: i64 = row.try_get(2).map_err(|_| Error::Integrity)?;
    if blob != receipt.blob || origin != 3 {
        return Err(Error::Conflict);
    }
    for column in [3, 6] {
        if sequence(row.try_get(column).map_err(|_| Error::Integrity)?)? > old {
            return Err(Error::Integrity);
        }
    }
    let length: i64 = row.try_get(4).map_err(|_| Error::Integrity)?;
    let digest = fixed(row.try_get(5).map_err(|_| Error::Integrity)?)?;
    let received_at = row.try_get(1).map_err(|_| Error::Integrity)?;
    let rows=client.query("SELECT m.mailbox_id,m.changed,i.source_object,i.changed FROM td_mta.memberships m LEFT JOIN td_mta.imports i ON i.account=m.account AND i.local_object=m.mailbox_id AND i.source_kind=1 AND i.source_instance=$3 AND i.source_account=$4 WHERE m.account=$1 AND m.email_id=$2 ORDER BY m.mailbox_id,i.source_object LIMIT 257",&[&account,&receipt.local.as_slice(),&key.instance.as_bytes().as_slice(),&key.account.as_slice()]).await.map_err(|_|Error::Database)?;
    if rows.is_empty() || rows.len() > 256 {
        return Err(Error::Conflict);
    }
    let mut mailboxes = Vec::new();
    let mut locals = std::collections::BTreeSet::new();
    for row in rows {
        let id: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
        if !locals.insert(fixed::<16>(id)?) {
            return Err(Error::Conflict);
        }
        let source: Option<&[u8]> = row.try_get(2).map_err(|_| Error::Integrity)?;
        mailboxes.push(source.ok_or(Error::Conflict)?.to_vec());
        for column in [1, 3] {
            if sequence(row.try_get(column).map_err(|_| Error::Integrity)?)? > old {
                return Err(Error::Integrity);
            }
        }
    }
    let rows=client.query("SELECT keyword,changed FROM td_mta.keywords WHERE account=$1 AND email_id=$2 ORDER BY keyword LIMIT 257",&[&account,&receipt.local.as_slice()]).await.map_err(|_|Error::Database)?;
    if rows.len() > 256 {
        return Err(Error::Conflict);
    }
    let mut keywords = Vec::new();
    for row in rows {
        if sequence(row.try_get(1).map_err(|_| Error::Integrity)?)? > old {
            return Err(Error::Integrity);
        }
        let keyword: &str = row.try_get(0).map_err(|_| Error::Integrity)?;
        keywords.push(keyword.to_owned());
    }
    let mailboxes: Vec<_> = mailboxes.iter().map(Vec::as_slice).collect();
    let keywords: Vec<_> = keywords.iter().map(String::as_str).collect();
    Email::new(
        key.clone(),
        EmailInput {
            body_digest: digest,
            length: u64::try_from(length).map_err(|_| Error::Integrity)?,
            received_at,
            mailboxes: &mailboxes,
            keywords: &keywords,
        },
    )
    .map(Some)
    .map_err(|_| Error::Conflict)
}
pub fn plan(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    authority: &Authority,
    value: &Email,
    definitions: &[crate::import_snapshot::Mailbox],
) -> Result<Plan, Error> {
    if request.mode != Mode::Preparation {
        return Err(Error::Owner);
    }
    if definitions.len() > 256 {
        return Err(Error::Capacity);
    }
    let authority = authority.clone();
    let value = value.clone();
    let definitions = definitions.to_vec();
    postgres_probe::with_reconcile_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let old = begin(client, &authority).await?;
            let Some(receipt) = receipt(client, &authority, &value.key, old).await? else {
                return Ok(Plan::New);
            };
            let current = match local(client, &authority, &value.key, &receipt, old).await {
                Ok(Some(local)) => local,
                Ok(None) => return Ok(Plan::Absent),
                Err(Error::Conflict) => return Ok(Plan::LocalChanged),
                Err(error) => return Err(error),
            };
            if current.digest != receipt.digest {
                return Ok(Plan::LocalChanged);
            }
            if current.body_digest != value.body_digest
                || current.length != value.length
                || current.received_at != value.received_at
            {
                return Ok(Plan::ImmutableChanged);
            }
            let fences = match folders(
                client,
                &authority,
                &value,
                &current,
                &definitions,
                None,
                old,
            )
            .await
            {
                Ok(fences) => fences,
                Err(Error::Conflict | Error::MissingMailbox) => return Ok(Plan::FolderChanged),
                Err(error) => return Err(error),
            };
            if receipt.digest == value.digest {
                return Ok(Plan::Current);
            }
            Ok(Plan::Update(Box::new(Baseline {
                epoch: authority.epoch,
                account: authority.account,
                key: value.key.clone(),
                local: receipt.local,
                historical_blob: receipt.blob,
                folders: fences,
                digest: receipt.digest,
                changed: receipt.changed,
                desired: value.digest,
            })))
        })
    })
    .map_err(Error::Connection)?
}
pub fn apply(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    authority: &Authority,
    value: &Email,
    definitions: &[crate::import_snapshot::Mailbox],
    expected: &Baseline,
) -> Result<Applied, Error> {
    if request.mode != Mode::Preparation {
        return Err(Error::Owner);
    }
    if expected.epoch != authority.epoch
        || expected.account != authority.account
        || expected.key != value.key
        || expected.desired != value.digest
    {
        return Err(Error::Invalid);
    }
    if definitions.len() > 256 || expected.folders.len() > 256 {
        return Err(Error::Capacity);
    }
    if expected
        .folders
        .iter()
        .any(|f| f.source.is_empty() || f.source.len() > 998)
    {
        return Err(Error::Invalid);
    }
    let authority = authority.clone();
    let value = value.clone();
    let expected = expected.clone();
    let definitions = definitions.to_vec();
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_reconcile_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let old = begin(client, &authority).await?;
            let receipt = receipt(client, &authority, &value.key, old)
                .await?
                .ok_or(Error::Conflict)?;
            if receipt.local != expected.local || receipt.blob != expected.historical_blob {
                return Err(Error::Conflict);
            }
            let present: bool = client
                .query_one(
                    "SELECT EXISTS(SELECT 1 FROM td_mta.emails WHERE account=$1 AND id=$2)",
                    &[
                        &authority.account.as_bytes().as_slice(),
                        &receipt.local.as_slice(),
                    ],
                )
                .await
                .map_err(|_| Error::Database)?
                .try_get(0)
                .map_err(|_| Error::Integrity)?;
            if !present {
                return Ok(Applied::Absent);
            }
            if receipt.digest == value.digest {
                return Ok(Applied::CurrentReceipt);
            }
            if receipt.digest != expected.digest || receipt.changed != expected.changed {
                return Err(Error::Conflict);
            }
            let current = local(client, &authority, &value.key, &receipt, old)
                .await?
                .ok_or(Error::Conflict)?;
            if current.digest != receipt.digest {
                return Err(Error::Conflict);
            }
            if current.body_digest != value.body_digest
                || current.length != value.length
                || current.received_at != value.received_at
            {
                return Err(Error::SourceChanged);
            }
            let fences = folders(
                client,
                &authority,
                &value,
                &current,
                &definitions,
                Some(&expected.folders),
                old,
            )
            .await?;
            let affected: std::collections::BTreeSet<_> = fences
                .iter()
                .filter(|f| {
                    current.mailboxes.contains(&f.source) || value.mailboxes.contains(&f.source)
                })
                .map(|f| f.local)
                .collect();
            let desired: Vec<_> = fences
                .iter()
                .filter(|f| value.mailboxes.contains(&f.source))
                .map(|f| f.local)
                .collect();
            let account = authority.account.as_bytes().as_slice();
            let id = receipt.local.as_slice();
            let next = next(client, &authority, old).await?;
            let changed = next.to_be_bytes();
            let changed = changed.as_slice();
            client
                .execute(
                    "DELETE FROM td_mta.memberships WHERE account=$1 AND email_id=$2",
                    &[&account, &id],
                )
                .await
                .map_err(|_| Error::Database)?;
            client
                .execute(
                    "DELETE FROM td_mta.keywords WHERE account=$1 AND email_id=$2",
                    &[&account, &id],
                )
                .await
                .map_err(|_| Error::Database)?;
            let boxes: Vec<_> = desired.iter().map(|id| id.as_slice()).collect();
            if client.execute("INSERT INTO td_mta.memberships SELECT $1,$2,id,$4 FROM unnest($3::bytea[]) ids(id)",&[&account,&id,&boxes,&changed]).await.map_err(|_|Error::Database)?!=boxes.len() as u64 {return Err(Error::Integrity);}
            let keywords: Vec<_> = value.keywords.iter().map(String::as_str).collect();
            if client.execute("INSERT INTO td_mta.keywords SELECT $1,$2,keyword,$4 FROM unnest($3::text[]) keys(keyword)",&[&account,&id,&keywords,&changed]).await.map_err(|_|Error::Database)?!=keywords.len() as u64 {return Err(Error::Integrity);}
            if client
                .execute(
                    "UPDATE td_mta.emails SET changed=$3 WHERE account=$1 AND id=$2",
                    &[&account, &id, &changed],
                )
                .await
                .map_err(|_| Error::Database)?
                != 1
            {
                return Err(Error::Integrity);
            }
            history(client, authority.account, next, 0, 3, 2, &receipt.local).await?;
            let boxes: Vec<_> = affected.iter().map(|id| id.as_slice()).collect();
            if client.execute("WITH updated AS (UPDATE td_mta.mailboxes SET changed=$3 WHERE account=$1 AND id=ANY($2::bytea[]) RETURNING id) INSERT INTO td_mta.changes SELECT $1,$3,row_number() OVER (ORDER BY id),1,2,id FROM updated",&[&account,&boxes,&changed]).await.map_err(|_|Error::Database)?!=boxes.len() as u64 {return Err(Error::Integrity);}
            if client.execute("UPDATE td_mta.imports SET source_digest=$5,changed=$6 WHERE account=$1 AND source_instance=$2 AND source_kind=3 AND source_account=$3 AND source_object=$4",&[&account,&value.key.instance.as_bytes().as_slice(),&value.key.account.as_slice(),&value.key.object.as_slice(),&value.digest.as_slice(),&changed]).await.map_err(|_|Error::Database)?!=1 {return Err(Error::Integrity);}
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
            commit(client, &marker).await?;
            Ok(Applied::Updated)
        })
    });
    settled(result, &attempted)
}

pub use super::folders::FolderFence;
async fn folders(
    client: &Client,
    authority: &Authority,
    value: &Email,
    current: &Email,
    definitions: &[crate::import_snapshot::Mailbox],
    expected: Option<&[FolderFence]>,
    old: u64,
) -> Result<Vec<FolderFence>, Error> {
    let sources: Vec<_> = current
        .mailboxes
        .iter()
        .chain(&value.mailboxes)
        .cloned()
        .collect();
    super::folders::check(
        client,
        authority,
        &value.key,
        &sources,
        definitions,
        expected,
        old,
    )
    .await
}
