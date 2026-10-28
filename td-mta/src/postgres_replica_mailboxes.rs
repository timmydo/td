//! Bounded source graph publication with deferred local removal dependencies.
use crate::{
    format::{
        key::Key,
        row::{MailboxRow, Row as MailRow},
    },
    ids::MailboxId,
    postgres_replica_apply::Error,
    postgres_replica_progress,
    postgres_replica_source::Source,
    replica_mailboxes::Graph,
};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};
use tokio_postgres::{types::Type, Client, Row};
const MAX: usize = crate::postgres_mailboxes::MAX_MAILBOXES;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Outcome {
    pub changed: bool,
    pub pending_removals: usize,
    pub local_sequence: u64,
}
struct Stored<'a> {
    id: MailboxId,
    row: MailboxRow<'a>,
    observed: u64,
    present: bool,
    referenced: bool,
}
struct Change<'a> {
    id: MailboxId,
    row: Option<MailboxRow<'a>>,
    action: i64,
}
fn bytes<const N: usize>(row: &Row, index: usize) -> Result<[u8; N], Error> {
    let bytes: &[u8] = row.try_get(index).map_err(|_| Error::Integrity)?;
    bytes.try_into().map_err(|_| Error::Integrity)
}
fn decode(row: &Row, sequence: u64) -> Result<Stored<'_>, Error> {
    let id = MailboxId::from_bytes(bytes(row, 0)?);
    let parent: Option<&[u8]> = row.try_get(2).map_err(|_| Error::Integrity)?;
    let parent = parent
        .map(|p| {
            p.try_into()
                .map(MailboxId::from_bytes)
                .map_err(|_| Error::Integrity)
        })
        .transpose()?;
    let subscribed: i64 = row.try_get(5).map_err(|_| Error::Integrity)?;
    if !matches!(subscribed, 0 | 1) || u64::from_be_bytes(bytes(row, 6)?) > sequence {
        return Err(Error::Integrity);
    }
    let value = MailboxRow {
        name: row.try_get(1).map_err(|_| Error::Integrity)?,
        parent,
        role: row.try_get(3).map_err(|_| Error::Integrity)?,
        sort_order: u32::try_from(row.try_get::<_, i64>(4).map_err(|_| Error::Integrity)?)
            .map_err(|_| Error::Integrity)?,
        subscribed: subscribed == 1,
    };
    MailRow::Mailbox(value)
        .validate_key(Key::Mailbox(id))
        .map_err(|_| Error::Integrity)?;
    Ok(Stored {
        id,
        row: value,
        observed: u64::from_be_bytes(bytes(row, 7)?),
        present: row.try_get(8).map_err(|_| Error::Integrity)?,
        referenced: row.try_get(9).map_err(|_| Error::Integrity)?,
    })
}
fn same(a: MailboxRow<'_>, b: MailboxRow<'_>) -> bool {
    a.name == b.name
        && a.parent == b.parent
        && a.role == b.role
        && a.sort_order == b.sort_order
        && a.subscribed == b.subscribed
}
fn reserve<T>(n: usize) -> Result<Vec<T>, Error> {
    let mut values = Vec::new();
    values.try_reserve_exact(n).map_err(|_| Error::Capacity)?;
    Ok(values)
}

pub(crate) async fn publish(
    client: &Client,
    graph: &Graph,
    deadline: Instant,
    marker: &AtomicBool,
) -> Result<Outcome, Error> {
    let account = graph.account.as_bytes().as_slice();
    let row = client
        .query_typed_opt(
            "SELECT sequence,floor FROM td_mta.accounts WHERE id=$1 FOR UPDATE",
            &[(&account, Type::BYTEA)],
        )
        .await
        .map_err(|_| Error::Database)?
        .ok_or(Error::Missing)?;
    let old = u64::from_be_bytes(bytes(&row, 0)?);
    if u64::from_be_bytes(bytes(&row, 1)?) > old {
        return Err(Error::Integrity);
    }
    let source =
        Source::new(graph.source.origin(), graph.source.epoch()).map_err(|_| Error::Source)?;
    let progress = postgres_replica_progress::read_snapshot(client, source, graph.account)
        .await
        .map_err(|_| Error::Integrity)?;
    let observed = graph.observed.sequence.number();
    if progress.generation != graph.generation
        || observed < progress.inventory_base.sequence.number()
    {
        return Err(Error::Stale);
    }
    let row = client
        .query_typed_one(
            "SELECT inbox FROM td_mta.account_bootstraps WHERE account=$1",
            &[(&account, Type::BYTEA)],
        )
        .await
        .map_err(|_| Error::Database)?;
    if bytes::<16>(&row, 0)? != *graph.inbox.as_bytes() {
        return Err(Error::Integrity);
    }
    let row=client.query_typed_opt("SELECT observed FROM td_mta.replica_mailbox_receipts WHERE account=$1 ORDER BY observed DESC LIMIT 1",&[(&account,Type::BYTEA)]).await.map_err(|_|Error::Database)?.ok_or(Error::Integrity)?;
    let latest = u64::from_be_bytes(bytes(&row, 0)?);
    if latest > observed {
        return Err(Error::Stale);
    }
    if latest < progress.mailbox_checkpoint.sequence.number() {
        return Err(Error::Integrity);
    }
    let rows=client.query_typed("SELECT m.id,m.name,m.parent_id,m.role,m.sort_order,m.subscribed,m.changed,r.observed,r.present,EXISTS(SELECT 1 FROM td_mta.memberships x WHERE x.account=m.account AND x.mailbox_id=m.id) FROM td_mta.mailboxes m LEFT JOIN td_mta.replica_mailbox_receipts r ON r.account=m.account AND r.id=m.id WHERE m.account=$1 ORDER BY m.id LIMIT 257",&[(&account,Type::BYTEA)]).await.map_err(|_|Error::Database)?;
    if rows.len() > MAX {
        return Err(Error::Capacity);
    }
    let mut stored = reserve(rows.len())?;
    for row in &rows {
        let value = decode(row, old)?;
        if value.observed > latest {
            return Err(Error::Integrity);
        }
        stored.push(value);
    }
    let mut old_hierarchy = reserve(stored.len())?;
    old_hierarchy.extend(stored.iter().map(|m| (m.id, m.row.parent)));
    crate::postgres_mailboxes::hierarchy_ids(&old_hierarchy).map_err(|_| Error::Integrity)?;
    let view = graph.page.view().map_err(|_| Error::Integrity)?;
    let find = |id| {
        view.list
            .binary_search_by_key(&id, |m| m.id)
            .ok()
            .and_then(|i| view.list.get(i))
    };
    let mut ids = reserve(view.list.len())?;
    ids.extend(view.list.iter().map(|m| m.id.as_bytes().as_slice()));
    let present=client.query_typed("SELECT id FROM td_mta.replica_mailbox_receipts WHERE account=$1 AND present ORDER BY id LIMIT 257",&[(&account,Type::BYTEA)]).await.map_err(|_|Error::Database)?;
    if present.len() > MAX || present.len() != stored.iter().filter(|m| m.present).count() {
        return Err(Error::Integrity);
    }
    for row in &present {
        let id = MailboxId::from_bytes(bytes(row, 0)?);
        if !stored.iter().any(|m| m.id == id && m.present) {
            return Err(Error::Integrity);
        }
    }
    if latest == observed
        && (view.list.len() != present.len()
            || stored
                .iter()
                .filter(|m| m.present)
                .any(|m| find(m.id).is_none_or(|other| !same(m.row, other.row))))
    {
        return Err(Error::Integrity);
    }
    let mut retained = reserve(MAX)?;
    for m in &stored {
        if find(m.id).is_none() && m.referenced {
            retained.push(m.id);
        }
    }
    let mut at = 0;
    while let Some(id) = retained.get(at).copied() {
        let m = stored.iter().find(|m| m.id == id).ok_or(Error::Integrity)?;
        if let Some(parent) = m.row.parent {
            if find(parent).is_none() && !retained.contains(&parent) {
                if retained.len() == MAX {
                    return Err(Error::Integrity);
                }
                retained.push(parent);
            }
        }
        at += 1;
    }
    if view
        .list
        .len()
        .checked_add(retained.len())
        .ok_or(Error::Capacity)?
        > MAX
    {
        return Err(Error::Capacity);
    }
    let mut changes = reserve(MAX * 2)?;
    let mut hierarchy = reserve(MAX)?;
    for m in &view.list {
        hierarchy.push((m.id, m.row.parent));
        match stored.iter().find(|old| old.id == m.id) {
            None => changes.push(Change {
                id: m.id,
                row: Some(m.row),
                action: 1,
            }),
            Some(old) if !same(old.row, m.row) => changes.push(Change {
                id: m.id,
                row: Some(m.row),
                action: 2,
            }),
            _ => {}
        }
    }
    for m in &stored {
        if find(m.id).is_some() {
            continue;
        }
        if retained.contains(&m.id) {
            hierarchy.push((m.id, m.row.parent));
            if m.row.role.is_some() {
                changes.push(Change {
                    id: m.id,
                    row: Some(MailboxRow {
                        role: None,
                        ..m.row
                    }),
                    action: 2,
                });
            }
        } else {
            changes.push(Change {
                id: m.id,
                row: None,
                action: 3,
            });
        }
    }
    hierarchy.sort_unstable_by_key(|m| m.0);
    crate::postgres_mailboxes::hierarchy_ids(&hierarchy).map_err(|_| Error::Integrity)?;
    let existing=client.query_typed_one("SELECT count(*) FROM td_mta.replica_mailbox_receipts WHERE account=$1 AND id=ANY($2::bytea[])",&[(&account,Type::BYTEA),(&ids,Type::BYTEA_ARRAY)]).await.map_err(|_|Error::Database)?.try_get::<_,i64>(0).map_err(|_|Error::Integrity)?;
    let extra = ids
        .len()
        .checked_sub(usize::try_from(existing).map_err(|_| Error::Integrity)?)
        .ok_or(Error::Integrity)?;
    let receipts = progress
        .receipt_count
        .checked_add(extra as u64)
        .filter(|count| *count <= progress.limits.receipts)
        .ok_or(Error::Quota)?;
    let next = if changes.is_empty() {
        old
    } else {
        old.checked_add(1).ok_or(Error::Capacity)?
    };
    write_changes(client, account, &changes, next).await?;
    client.execute_typed("UPDATE td_mta.replica_mailbox_receipts SET observed=$3,present=false WHERE account=$1 AND present AND NOT(id=ANY($2::bytea[]))",&[(&account,Type::BYTEA),(&ids,Type::BYTEA_ARRAY),(&observed.to_be_bytes().as_slice(),Type::BYTEA)]).await.map_err(|_|Error::Database)?;
    if client.execute_typed("INSERT INTO td_mta.replica_mailbox_receipts(account,id,observed,present) SELECT $1,id,$3,true FROM unnest($2::bytea[]) q(id) ON CONFLICT(account,id) DO UPDATE SET observed=EXCLUDED.observed,present=true",&[(&account,Type::BYTEA),(&ids,Type::BYTEA_ARRAY),(&observed.to_be_bytes().as_slice(),Type::BYTEA)]).await.map_err(|_|Error::Database)?!=ids.len() as u64 {return Err(Error::Integrity);}
    let checkpoint = if retained.is_empty() {
        observed
    } else {
        progress.mailbox_checkpoint.sequence.number()
    };
    if client.execute_typed("UPDATE td_mta.replica_progress SET receipt_count=$2,mailbox_checkpoint=$3 WHERE account=$1",&[(&account,Type::BYTEA),(&(receipts as i64),Type::INT8),(&checkpoint.to_be_bytes().as_slice(),Type::BYTEA)]).await.map_err(|_|Error::Database)?!=1 {return Err(Error::Integrity);}
    if next != old
        && client
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
    if Instant::now() >= deadline {
        return Err(Error::Connection(crate::postgres_probe::Error::Deadline));
    }
    marker.store(true, Ordering::Relaxed);
    client
        .batch_execute("COMMIT")
        .await
        .map_err(|_| Error::Indeterminate)?;
    Ok(Outcome {
        changed: next != old,
        pending_removals: retained.len(),
        local_sequence: next,
    })
}
async fn write_changes(
    client: &Client,
    account: &[u8],
    changes: &[Change<'_>],
    next: u64,
) -> Result<(), Error> {
    if changes.is_empty() {
        return Ok(());
    }
    let mut ids = reserve(changes.len())?;
    let mut actions = reserve(changes.len())?;
    let mut deleted = reserve(changes.len())?;
    let mut written = reserve(MAX)?;
    let mut names = reserve(MAX)?;
    let mut parents = reserve(MAX)?;
    let mut roles = reserve(MAX)?;
    let mut sorts = reserve(MAX)?;
    let mut subscriptions = reserve(MAX)?;
    for change in changes {
        ids.push(change.id.as_bytes().as_slice());
        actions.push(change.action);
        if let Some(row) = change.row {
            written.push(change.id.as_bytes().as_slice());
            names.push(row.name);
            parents.push(row.parent.map(|id| *id.as_bytes()));
            roles.push(row.role);
            sorts.push(i64::from(row.sort_order));
            subscriptions.push(i64::from(row.subscribed));
        } else {
            deleted.push(change.id.as_bytes().as_slice());
        }
    }
    let mut parent_values = reserve(parents.len())?;
    parent_values.extend(parents.iter().map(|id| id.as_ref().map(|id| id.as_slice())));
    if !written.is_empty() && client.execute_typed("INSERT INTO td_mta.mailboxes(account,id,name,parent_id,role,sort_order,subscribed,changed) SELECT $1,id,name,parent_id,role,sort_order,subscribed,$2 FROM unnest($3::bytea[],$4::text[],$5::bytea[],$6::text[],$7::bigint[],$8::bigint[]) q(id,name,parent_id,role,sort_order,subscribed) ON CONFLICT(account,id) DO UPDATE SET name=EXCLUDED.name,parent_id=EXCLUDED.parent_id,role=EXCLUDED.role,sort_order=EXCLUDED.sort_order,subscribed=EXCLUDED.subscribed,changed=EXCLUDED.changed",&[(&account,Type::BYTEA),(&next.to_be_bytes().as_slice(),Type::BYTEA),(&written,Type::BYTEA_ARRAY),(&names,Type::TEXT_ARRAY),(&parent_values,Type::BYTEA_ARRAY),(&roles,Type::TEXT_ARRAY),(&sorts,Type::INT8_ARRAY),(&subscriptions,Type::INT8_ARRAY)]).await.map_err(|_|Error::Database)?!=written.len() as u64 {return Err(Error::Integrity);}
    if !deleted.is_empty()
        && client
            .execute_typed(
                "DELETE FROM td_mta.mailboxes WHERE account=$1 AND id=ANY($2::bytea[])",
                &[(&account, Type::BYTEA), (&deleted, Type::BYTEA_ARRAY)],
            )
            .await
            .map_err(|_| Error::Database)?
            != deleted.len() as u64
    {
        return Err(Error::Integrity);
    }
    if client.execute_typed("INSERT INTO td_mta.changes(account,sequence,operation,kind,action,object) SELECT $1,$2,ordinality,1,action,id FROM unnest($3::bytea[],$4::bigint[]) WITH ORDINALITY q(id,action,ordinality)",&[(&account,Type::BYTEA),(&next.to_be_bytes().as_slice(),Type::BYTEA),(&ids,Type::BYTEA_ARRAY),(&actions,Type::INT8_ARRAY)]).await.map_err(|_|Error::Database)?!=ids.len() as u64 {return Err(Error::Integrity);}
    Ok(())
}
