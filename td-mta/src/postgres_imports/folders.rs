//! Validate required source folders and ancestors under the account lock.
use super::*;
#[derive(Clone)]
pub struct FolderFence {
    pub source: Vec<u8>,
    pub local: [u8; 16],
}
pub(super) async fn check(
    client: &Client,
    authority: &Authority,
    key: &Key,
    requested: &[Vec<u8>],
    definitions: &[crate::import_snapshot::Mailbox],
    expected: Option<&[FolderFence]>,
    old: u64,
) -> Result<Vec<FolderFence>, Error> {
    use crate::import_snapshot::{Mailbox, MailboxInput};
    let mut sources: std::collections::BTreeSet<&[u8]> =
        requested.iter().map(Vec::as_slice).collect();
    if sources.len() > 256 || definitions.len() > 256 {
        return Err(Error::Capacity);
    }
    let mut wanted = std::collections::BTreeMap::new();
    for mailbox in definitions {
        if mailbox.key.instance != key.instance
            || mailbox.key.account != key.account
            || wanted
                .insert(mailbox.key.object.as_slice(), mailbox)
                .is_some()
        {
            return Err(Error::Invalid);
        }
    }
    let mut resolved = std::collections::BTreeSet::new();
    for source in sources.clone() {
        let mut path = std::collections::BTreeSet::new();
        let mut at = Some(source);
        while let Some(id) = at {
            if resolved.contains(id) {
                break;
            }
            if !path.insert(id) {
                return Err(Error::Conflict);
            }
            let definition = wanted.get(id).ok_or(Error::MissingMailbox)?;
            sources.insert(id);
            at = definition.parent.as_deref();
        }
        resolved.extend(path);
    }
    if sources.len() > 256 {
        return Err(Error::Capacity);
    }
    let sources: Vec<_> = sources.into_iter().collect();
    let account = authority.account.as_bytes().as_slice();
    let rows=client.query("SELECT i.source_object,i.local_object,i.source_digest,i.changed,m.name,m.parent_id,m.role,m.sort_order,m.subscribed,m.changed,p.source_object,NOT EXISTS(SELECT 1 FROM td_mta.imports a WHERE a.account=i.account AND a.source_instance=i.source_instance AND a.source_kind=1 AND a.source_account=i.source_account AND a.local_object=i.local_object AND a.source_object<>i.source_object) FROM unnest($4::bytea[]) requested(id) LEFT JOIN td_mta.imports i ON i.account=$1 AND i.source_instance=$2 AND i.source_kind=1 AND i.source_account=$3 AND i.source_object=requested.id LEFT JOIN td_mta.mailboxes m ON m.account=i.account AND m.id=i.local_object LEFT JOIN td_mta.imports p ON p.account=m.account AND p.local_object=m.parent_id AND p.source_instance=$2 AND p.source_kind=1 AND p.source_account=$3 ORDER BY i.source_object LIMIT 257",&[&account,&key.instance.as_bytes().as_slice(),&key.account.as_slice(),&sources]).await.map_err(|_|Error::Database)?;
    if rows.len() != sources.len() {
        return Err(Error::Conflict);
    }
    let mut fences = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut locals = std::collections::BTreeSet::new();
    for row in rows {
        let source: Option<&[u8]> = row.try_get(0).map_err(|_| Error::Integrity)?;
        let source = source.ok_or(Error::MissingMailbox)?;
        if !seen.insert(source.to_vec()) {
            return Err(Error::Conflict);
        }
        if !row.try_get::<_, bool>(11).map_err(|_| Error::Integrity)? {
            return Err(Error::Conflict);
        }
        let id = fixed::<16>(row.try_get(1).map_err(|_| Error::Integrity)?)?;
        if !locals.insert(id) {
            return Err(Error::Conflict);
        }
        let name: Option<&str> = row.try_get(4).map_err(|_| Error::Integrity)?;
        let name = name.ok_or(Error::MissingMailbox)?;
        for column in [3, 9] {
            if sequence(row.try_get(column).map_err(|_| Error::Integrity)?)? > old {
                return Err(Error::Integrity);
            }
        }
        let parent: Option<&[u8]> = row.try_get(5).map_err(|_| Error::Integrity)?;
        let parent_source: Option<&[u8]> = row.try_get(10).map_err(|_| Error::Integrity)?;
        if parent.is_some() != parent_source.is_some() {
            return Err(Error::Conflict);
        }
        let sort: i64 = row.try_get(7).map_err(|_| Error::Integrity)?;
        let subscribed: i64 = row.try_get(8).map_err(|_| Error::Integrity)?;
        if !matches!(subscribed, 0 | 1) {
            return Err(Error::Integrity);
        }
        let wanted = wanted.get(source).ok_or(Error::Integrity)?;
        let current = Mailbox::new(
            wanted.key.clone(),
            MailboxInput {
                name,
                parent: parent_source,
                role: row.try_get(6).map_err(|_| Error::Integrity)?,
                sort_order: u32::try_from(sort).map_err(|_| Error::Integrity)?,
                subscribed: subscribed == 1,
            },
        )
        .map_err(|_| Error::Integrity)?;
        let receipt: &[u8] = row.try_get(2).map_err(|_| Error::Integrity)?;
        if receipt != wanted.digest || current.digest != wanted.digest {
            return Err(Error::Conflict);
        }
        if let Some(expected) = expected {
            if expected.len() != sources.len()
                || expected
                    .iter()
                    .filter(|e| e.source == source && e.local == id)
                    .count()
                    != 1
            {
                return Err(Error::Conflict);
            }
        }
        fences.push(FolderFence {
            source: source.to_vec(),
            local: id,
        });
    }
    Ok(fences)
}
