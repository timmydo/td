//! Bounded value prefixes from the current immutable snapshot.
use super::{check_changed, copy_text, Error, Values};
use crate::{
    format::key::Key,
    ids::{EmailId, MailboxId},
    ports::ViewIdentity,
};
use tokio_postgres::{types::Type, Client};

const VALUES: usize = 9;
const _: () = assert!(VALUES == 9 && crate::postgres_body::MAX_METADATA_IDS == 16);

#[derive(Default)]
pub(super) struct Window {
    entries: Vec<Entry>,
}
struct Entry {
    email: EmailId,
    mailboxes: Option<Prefix<MailboxId>>,
    keywords: Option<Prefix<String>>,
}
struct Prefix<T> {
    values: Vec<T>,
    has_more: bool,
}
impl Window {
    pub(super) async fn read(
        client: &Client,
        identity: ViewIdentity,
        emails: &[EmailId],
        mailboxes: bool,
        keywords: bool,
    ) -> Result<Self, Error> {
        if emails.len() > crate::postgres_body::MAX_METADATA_IDS {
            return Err(Error::Capacity);
        }
        let mut window = Self::default();
        window
            .entries
            .try_reserve_exact(emails.len())
            .map_err(|_| Error::Capacity)?;
        let mut ids = Vec::new();
        ids.try_reserve_exact(emails.len())
            .map_err(|_| Error::Capacity)?;
        for email in emails {
            if ids.contains(&email.as_bytes().as_slice()) {
                continue;
            }
            ids.push(email.as_bytes().as_slice());
            window.entries.push(Entry {
                email: *email,
                mailboxes: None,
                keywords: None,
            });
        }
        if ids.is_empty() {
            return Ok(window);
        }
        // Each bounded prefix occupies one protocol row, not one row per value.
        if mailboxes {
            let rows = client.query_typed(
                "SELECT i.id,v.ids,v.changes,v.parents FROM unnest($2::bytea[]) i(id) CROSS JOIN LATERAL (SELECT COALESCE(array_agg(x.mailbox_id ORDER BY x.mailbox_id),'{}'::bytea[]) AS ids,COALESCE(array_agg(x.changed ORDER BY x.mailbox_id),'{}'::bytea[]) AS changes,COALESCE(array_agg(x.mailbox_changed ORDER BY x.mailbox_id),'{}'::bytea[]) AS parents FROM (SELECT m.mailbox_id,m.changed,b.changed AS mailbox_changed FROM td_mta.memberships m LEFT JOIN td_mta.mailboxes b ON b.account=m.account AND b.id=m.mailbox_id WHERE m.account=$1 AND m.email_id=i.id ORDER BY m.mailbox_id LIMIT 9) x) v ORDER BY i.id LIMIT 17",
                &[(&identity.account.as_bytes().as_slice(), Type::BYTEA), (&ids, Type::BYTEA_ARRAY)],
            ).await.map_err(|_| Error::Database)?;
            if rows.len() != window.entries.len() {
                return Err(Error::Integrity);
            }
            let mut previous = None;
            for row in rows {
                let email: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
                let email = EmailId::from_bytes(email.try_into().map_err(|_| Error::Integrity)?);
                if previous.is_some_and(|id| id >= email) {
                    return Err(Error::Integrity);
                }
                previous = Some(email);
                let entry = window
                    .entries
                    .iter_mut()
                    .find(|entry| entry.email == email)
                    .ok_or(Error::Integrity)?;
                let raw: Vec<Vec<u8>> = row.try_get(1).map_err(|_| Error::Integrity)?;
                let changes: Vec<Vec<u8>> = row.try_get(2).map_err(|_| Error::Integrity)?;
                let parents: Vec<Vec<u8>> = row.try_get(3).map_err(|_| Error::Integrity)?;
                if raw.len() > VALUES || changes.len() != raw.len() || parents.len() != raw.len() {
                    return Err(Error::Integrity);
                }
                let mut values = Vec::new();
                values
                    .try_reserve_exact(raw.len())
                    .map_err(|_| Error::Capacity)?;
                for ((id, changed), parent) in raw.iter().zip(&changes).zip(&parents) {
                    check_changed(changed, identity)?;
                    check_changed(parent, identity)?;
                    let id = MailboxId::from_bytes(
                        id.as_slice().try_into().map_err(|_| Error::Integrity)?,
                    );
                    if values.last().is_some_and(|last| *last >= id) {
                        return Err(Error::Integrity);
                    }
                    values.push(id);
                }
                entry.mailboxes = Some(Prefix {
                    has_more: values.len() == VALUES,
                    values,
                });
            }
        }
        if keywords {
            let rows = client.query_typed(
                "SELECT i.id,v.words,v.changes FROM unnest($2::bytea[]) i(id) CROSS JOIN LATERAL (SELECT COALESCE(array_agg(x.keyword ORDER BY x.keyword COLLATE \"C\"),'{}'::text[]) AS words,COALESCE(array_agg(x.changed ORDER BY x.keyword COLLATE \"C\"),'{}'::bytea[]) AS changes FROM (SELECT keyword,changed FROM td_mta.keywords WHERE account=$1 AND email_id=i.id ORDER BY keyword COLLATE \"C\" LIMIT 9) x) v ORDER BY i.id LIMIT 17",
                &[(&identity.account.as_bytes().as_slice(), Type::BYTEA), (&ids, Type::BYTEA_ARRAY)],
            ).await.map_err(|_| Error::Database)?;
            if rows.len() != window.entries.len() {
                return Err(Error::Integrity);
            }
            let mut previous = None;
            for row in rows {
                let email: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
                let email = EmailId::from_bytes(email.try_into().map_err(|_| Error::Integrity)?);
                if previous.is_some_and(|id| id >= email) {
                    return Err(Error::Integrity);
                }
                previous = Some(email);
                let entry = window
                    .entries
                    .iter_mut()
                    .find(|entry| entry.email == email)
                    .ok_or(Error::Integrity)?;
                let values: Vec<String> = row.try_get(1).map_err(|_| Error::Integrity)?;
                let changes: Vec<Vec<u8>> = row.try_get(2).map_err(|_| Error::Integrity)?;
                if values.len() > VALUES || values.len() != changes.len() {
                    return Err(Error::Integrity);
                }
                let mut previous = None;
                for (value, changed) in values.iter().zip(&changes) {
                    check_changed(changed, identity)?;
                    Key::Keyword(email, value)
                        .validate_local()
                        .map_err(|_| Error::Integrity)?;
                    if previous.is_some_and(|last: &str| last >= value.as_str()) {
                        return Err(Error::Integrity);
                    }
                    previous = Some(value.as_str());
                }
                entry.keywords = Some(Prefix {
                    has_more: values.len() == VALUES,
                    values,
                });
            }
        }
        Ok(window)
    }
    pub(super) fn mailboxes(
        &self,
        identity: ViewIdentity,
        email: EmailId,
        after: Option<MailboxId>,
        maximum: usize,
    ) -> Result<Option<Values<MailboxId>>, Error> {
        let Some(values) = self
            .entries
            .iter()
            .find(|entry| entry.email == email)
            .and_then(|entry| entry.mailboxes.as_ref())
        else {
            return Ok(None);
        };
        if values.has_more
            && after.is_some_and(|after| values.values.last().is_none_or(|last| after >= *last))
        {
            return Ok(None);
        }
        page(
            identity,
            email,
            values
                .values
                .iter()
                .filter(|value| after.is_none_or(|after| **value > after)),
            maximum,
            values.has_more,
            |value| Ok(*value),
        )
        .map(Some)
    }
    pub(super) fn keywords(
        &self,
        identity: ViewIdentity,
        email: EmailId,
        after: Option<&str>,
        maximum: usize,
    ) -> Result<Option<Values<String>>, Error> {
        let Some(values) = self
            .entries
            .iter()
            .find(|entry| entry.email == email)
            .and_then(|entry| entry.keywords.as_ref())
        else {
            return Ok(None);
        };
        if values.has_more
            && after.is_some_and(|after| {
                values
                    .values
                    .last()
                    .is_none_or(|last| after >= last.as_str())
            })
        {
            return Ok(None);
        }
        page(
            identity,
            email,
            values
                .values
                .iter()
                .filter(|value| after.is_none_or(|after| value.as_str() > after)),
            maximum,
            values.has_more,
            |value| copy_text(value),
        )
        .map(Some)
    }
}
fn page<'a, T: 'a>(
    identity: ViewIdentity,
    email: EmailId,
    mut items: impl Iterator<Item = &'a T>,
    maximum: usize,
    known_more: bool,
    copy: impl Fn(&T) -> Result<T, Error>,
) -> Result<Values<T>, Error> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(maximum.min(VALUES))
        .map_err(|_| Error::Capacity)?;
    for value in items.by_ref().take(maximum) {
        values.push(copy(value)?);
    }
    Ok(Values {
        identity,
        email,
        values,
        has_more: known_more || items.next().is_some(),
    })
}
