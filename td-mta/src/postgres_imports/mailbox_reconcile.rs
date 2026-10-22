//! Retained owner plans for in-place source folder metadata updates.
pub use super::folders::FolderFence;
pub use super::reconcile::Applied;
use super::*;
use crate::{
    ids::MailboxId,
    import_snapshot::{Mailbox, MailboxInput},
};
use std::sync::{atomic::AtomicBool, Arc};
#[derive(Clone)]
pub struct Baseline {
    pub epoch: StoreEpoch,
    pub account: AccountId,
    pub key: Key,
    pub local: [u8; 16],
    pub digest: [u8; 32],
    pub changed: u64,
    pub desired: [u8; 32],
    pub folders: Vec<FolderFence>,
}
pub enum Plan {
    New,
    Current,
    Absent,
    LocalChanged,
    RoleChanged,
    FolderChanged,
    Update(Box<Baseline>),
}
struct Receipt {
    local: [u8; 16],
    digest: [u8; 32],
    changed: u64,
}
async fn receipt(
    client: &Client,
    authority: &Authority,
    key: &Key,
    old: u64,
) -> Result<Option<Receipt>, Error> {
    let row=client.query_opt("SELECT local_object,historical_blob,source_digest,changed FROM td_mta.imports WHERE account=$1 AND source_instance=$2 AND source_kind=1 AND source_account=$3 AND source_object=$4",&[&authority.account.as_bytes().as_slice(),&key.instance.as_bytes().as_slice(),&key.account.as_slice(),&key.object.as_slice()]).await.map_err(|_|Error::Database)?;
    let Some(row) = row else { return Ok(None) };
    let blob: Option<&[u8]> = row.try_get(1).map_err(|_| Error::Integrity)?;
    let changed = sequence(row.try_get(3).map_err(|_| Error::Integrity)?)?;
    if blob.is_some() || changed > old {
        return Err(Error::Integrity);
    }
    Ok(Some(Receipt {
        local: fixed(row.try_get(0).map_err(|_| Error::Integrity)?)?,
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
) -> Result<Option<Mailbox>, Error> {
    let rows=client.query("SELECT m.name,m.parent_id,m.role,m.sort_order,m.subscribed,m.changed,p.source_object,p.changed,NOT EXISTS(SELECT 1 FROM td_mta.imports a WHERE a.account=m.account AND a.source_kind=1 AND a.local_object=m.id AND (a.source_instance<>$3 OR a.source_account<>$4 OR a.source_object<>$5)) FROM td_mta.mailboxes m LEFT JOIN td_mta.imports p ON p.account=m.account AND p.local_object=m.parent_id AND p.source_kind=1 AND p.source_instance=$3 AND p.source_account=$4 WHERE m.account=$1 AND m.id=$2 LIMIT 2",&[&authority.account.as_bytes().as_slice(),&receipt.local.as_slice(),&key.instance.as_bytes().as_slice(),&key.account.as_slice(),&key.object.as_slice()]).await.map_err(|_|Error::Database)?;
    if rows.len() > 1 {
        return Err(Error::Conflict);
    }
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    if !row.try_get::<_, bool>(8).map_err(|_| Error::Integrity)? {
        return Err(Error::Conflict);
    }
    let parent: Option<&[u8]> = row.try_get(1).map_err(|_| Error::Integrity)?;
    let parent_source: Option<&[u8]> = row.try_get(6).map_err(|_| Error::Integrity)?;
    if parent.is_some() != parent_source.is_some() {
        return Err(Error::Conflict);
    }
    if let Some(parent) = parent {
        fixed::<16>(parent)?;
        if sequence(row.try_get(7).map_err(|_| Error::Integrity)?)? > old {
            return Err(Error::Integrity);
        }
    }
    if sequence(row.try_get(5).map_err(|_| Error::Integrity)?)? > old {
        return Err(Error::Integrity);
    }
    let sort: i64 = row.try_get(3).map_err(|_| Error::Integrity)?;
    let subscribed: i64 = row.try_get(4).map_err(|_| Error::Integrity)?;
    if !matches!(subscribed, 0 | 1) {
        return Err(Error::Integrity);
    }
    Mailbox::new(
        key.clone(),
        MailboxInput {
            name: row.try_get(0).map_err(|_| Error::Integrity)?,
            parent: parent_source,
            role: row.try_get(2).map_err(|_| Error::Integrity)?,
            sort_order: u32::try_from(sort).map_err(|_| Error::Integrity)?,
            subscribed: subscribed == 1,
        },
    )
    .map(Some)
    .map_err(|_| Error::Conflict)
}
async fn destination(
    client: &Client,
    authority: &Authority,
    value: &Mailbox,
    definitions: &[Mailbox],
    expected: Option<&[FolderFence]>,
    old: u64,
) -> Result<Vec<FolderFence>, Error> {
    let required: Vec<_> = value.parent.iter().cloned().collect();
    let fences = super::folders::check(
        client,
        authority,
        &value.key,
        &required,
        definitions,
        expected,
        old,
    )
    .await?;
    if expected.is_some_and(|e| e.len() != fences.len()) {
        return Err(Error::Conflict);
    }
    Ok(fences)
}
fn parent(value: &Mailbox, fences: &[FolderFence]) -> Result<Option<[u8; 16]>, Error> {
    value
        .parent
        .as_ref()
        .map(|source| {
            fences
                .iter()
                .find(|f| &f.source == source)
                .map(|f| f.local)
                .ok_or(Error::MissingMailbox)
        })
        .transpose()
}
async fn candidate(
    client: &Client,
    authority: &Authority,
    value: &Mailbox,
    id: [u8; 16],
    parent: Option<[u8; 16]>,
    old: u64,
) -> Result<(), Error> {
    use crate::format::{
        key::Key as FormatKey,
        row::{MailboxRow, Row},
    };
    let rows=client.query("SELECT id,name,parent_id,role,sort_order,subscribed,changed FROM td_mta.mailboxes WHERE account=$1 ORDER BY id LIMIT 257",&[&authority.account.as_bytes().as_slice()]).await.map_err(|_|Error::Database)?;
    if rows.len() > crate::postgres_mailboxes::MAX_MAILBOXES {
        return Err(Error::Capacity);
    }
    let mut graph = Vec::new();
    let mut names = std::collections::BTreeSet::new();
    let mut roles = std::collections::BTreeSet::new();
    let mut found = false;
    for row in &rows {
        let local = fixed::<16>(row.try_get(0).map_err(|_| Error::Integrity)?)?;
        let stored_parent: Option<&[u8]> = row.try_get(2).map_err(|_| Error::Integrity)?;
        let stored_parent = stored_parent.map(fixed::<16>).transpose()?;
        let order: i64 = row.try_get(4).map_err(|_| Error::Integrity)?;
        let subscribed: i64 = row.try_get(5).map_err(|_| Error::Integrity)?;
        if !matches!(subscribed, 0 | 1)
            || sequence(row.try_get(6).map_err(|_| Error::Integrity)?)? > old
        {
            return Err(Error::Integrity);
        }
        let mut fields = MailboxRow {
            name: row.try_get(1).map_err(|_| Error::Integrity)?,
            parent: stored_parent.map(MailboxId::from_bytes),
            role: row.try_get(3).map_err(|_| Error::Integrity)?,
            sort_order: u32::try_from(order).map_err(|_| Error::Integrity)?,
            subscribed: subscribed == 1,
        };
        let local_id = MailboxId::from_bytes(local);
        Row::Mailbox(fields)
            .validate_key(FormatKey::Mailbox(local_id))
            .map_err(|_| Error::Integrity)?;
        if local == id {
            found = true;
            fields = MailboxRow {
                name: &value.name,
                parent: parent.map(MailboxId::from_bytes),
                role: value.role.as_deref(),
                sort_order: value.sort_order,
                subscribed: value.subscribed,
            };
        }
        if !names.insert((fields.parent, fields.name))
            || fields.role.is_some_and(|r| !roles.insert(r))
        {
            return Err(Error::Conflict);
        }
        graph.push((local_id, fields.parent));
    }
    if !found {
        return Err(Error::Conflict);
    }
    crate::postgres_mailboxes::hierarchy_ids(&graph).map_err(|_| Error::Conflict)
}
pub fn plan(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    authority: &Authority,
    value: &Mailbox,
    definitions: &[Mailbox],
) -> Result<Plan, Error> {
    if request.mode != Mode::Preparation {
        return Err(Error::Owner);
    }
    if definitions.len() > 256 {
        return Err(Error::Capacity);
    }
    super::mailbox::validate_name(&value.name, deadline)?;
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
                Ok(Some(current)) => current,
                Ok(None) => return Ok(Plan::Absent),
                Err(Error::Conflict) => return Ok(Plan::LocalChanged),
                Err(e) => return Err(e),
            };
            if current.digest != receipt.digest {
                return Ok(Plan::LocalChanged);
            }
            if current.role != value.role {
                return Ok(Plan::RoleChanged);
            }
            let fences =
                match destination(client, &authority, &value, &definitions, None, old).await {
                    Ok(fences) => fences,
                    Err(Error::Conflict | Error::MissingMailbox) => return Ok(Plan::FolderChanged),
                    Err(e) => return Err(e),
                };
            match candidate(
                client,
                &authority,
                &value,
                receipt.local,
                parent(&value, &fences)?,
                old,
            )
            .await
            {
                Ok(()) => {}
                Err(Error::Conflict) => return Ok(Plan::FolderChanged),
                Err(e) => return Err(e),
            }
            if receipt.digest == value.digest {
                return Ok(Plan::Current);
            }
            Ok(Plan::Update(Box::new(Baseline {
                epoch: authority.epoch,
                account: authority.account,
                key: value.key.clone(),
                local: receipt.local,
                digest: receipt.digest,
                changed: receipt.changed,
                desired: value.digest,
                folders: fences,
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
    value: &Mailbox,
    definitions: &[Mailbox],
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
    super::mailbox::validate_name(&value.name, deadline)?;
    let authority = authority.clone();
    let value = value.clone();
    let definitions = definitions.to_vec();
    let expected = expected.clone();
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_reconcile_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let old = begin(client, &authority).await?;
            let receipt = receipt(client, &authority, &value.key, old)
                .await?
                .ok_or(Error::Conflict)?;
            if receipt.local != expected.local {
                return Err(Error::Conflict);
            }
            let present: bool = client
                .query_one(
                    "SELECT EXISTS(SELECT 1 FROM td_mta.mailboxes WHERE account=$1 AND id=$2)",
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
            if current.role != value.role {
                return Err(Error::SourceChanged);
            }
            let fences = destination(
                client,
                &authority,
                &value,
                &definitions,
                Some(&expected.folders),
                old,
            )
            .await?;
            let parent = parent(&value, &fences)?;
            candidate(client, &authority, &value, receipt.local, parent, old).await?;
            let next = next(client, &authority, old).await?;
            let changed = next.to_be_bytes();
            let account = authority.account.as_bytes().as_slice();
            if client.execute("UPDATE td_mta.mailboxes SET name=$3,parent_id=$4,sort_order=$5,subscribed=$6,changed=$7 WHERE account=$1 AND id=$2",&[&account,&receipt.local.as_slice(),&value.name,&parent.as_ref().map(|p|p.as_slice()),&i64::from(value.sort_order),&i64::from(value.subscribed),&changed.as_slice()]).await.map_err(|_|Error::Database)?!=1{return Err(Error::Integrity)}
            history(client, authority.account, next, 0, 1, 2, &receipt.local).await?;
            if client.execute("UPDATE td_mta.imports SET source_digest=$5,changed=$6 WHERE account=$1 AND source_instance=$2 AND source_kind=1 AND source_account=$3 AND source_object=$4",&[&account,&value.key.instance.as_bytes().as_slice(),&value.key.account.as_slice(),&value.key.object.as_slice(),&value.digest.as_slice(),&changed.as_slice()]).await.map_err(|_|Error::Database)?!=1{return Err(Error::Integrity)}
            if client
                .execute(
                    "UPDATE td_mta.accounts SET sequence=$2 WHERE id=$1",
                    &[&account, &changed.as_slice()],
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
