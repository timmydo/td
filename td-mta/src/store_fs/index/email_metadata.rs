//! Native stored Email metadata and bounded membership/keyword windows.
use super::*;
pub use crate::email_response::{EmailMetadata, EmailValues, MAX_EMAIL_VALUES};
use crate::ids::EmailId;
const MAILBOX_FIRST: &str = "SELECT mailbox_id,changed FROM memberships WHERE account=?1 AND email_id=?2 ORDER BY mailbox_id LIMIT ?3";
const MAILBOX_AFTER: &str = "SELECT mailbox_id,changed FROM memberships WHERE account=?1 AND email_id=?2 AND mailbox_id>?3 ORDER BY mailbox_id LIMIT ?4";
const KEYWORD_FIRST: &str = "SELECT keyword,changed FROM keywords WHERE account=?1 AND email_id=?2 ORDER BY keyword LIMIT ?3";
const KEYWORD_AFTER: &str = "SELECT keyword,changed FROM keywords WHERE account=?1 AND email_id=?2 AND keyword>?3 ORDER BY keyword LIMIT ?4";
impl IndexReadView<'_, '_> {
    /// Read no MIME body bytes; scratch must fit both Email and Blob rows.
    /// format::MAX_VALUE_BYTES is sufficient, including SMTP receipts.
    pub fn email_metadata(
        &mut self,
        email: EmailId,
        scratch: &mut [u8],
    ) -> Result<Option<EmailMetadata>, ports::Error> {
        let identity = self.identity;
        self.read_snapshot(|native| {
            native.check()?;
            let Some((row, _)) = get(native, identity, Key::Email(email), scratch)? else {
                return Ok(None);
            };
            let Row::Email(row) = row else {
                return Err(ports::Error::Corrupt);
            };
            let (blob, thread, received_at) = (row.blob, row.thread, row.received_at);
            let Some((Row::Blob(row), _)) = get(native, identity, Key::Blob(blob), scratch)? else {
                return Err(ports::Error::Corrupt);
            };
            let value = EmailMetadata {
                identity,
                email,
                blob,
                thread,
                size: row.length,
                received_at,
            };
            native.check()?;
            Ok(Some(value))
        })
    }
    pub fn email_mailboxes(
        &mut self,
        email: EmailId,
        after: Option<MailboxId>,
        maximum: usize,
    ) -> Result<EmailValues<MailboxId>, ports::Error> {
        let after_bytes = after.map(|id| *id.as_bytes());
        self.email_values(
            email,
            after_bytes.as_ref().map(|bytes| bytes.as_slice()),
            maximum,
            (MAILBOX_FIRST, MAILBOX_AFTER),
            |row, retain| {
                let value = MailboxId::from_bytes(fixed_blob::<16>(row, 0)?);
                Ok(retain.then_some(value))
            },
        )
    }
    pub fn email_keywords(
        &mut self,
        email: EmailId,
        after: Option<&str>,
        maximum: usize,
    ) -> Result<EmailValues<String>, ports::Error> {
        if let Some(after) = after {
            Key::Keyword(email, after)
                .validate_local()
                .map_err(|_| ports::Error::Invalid)?;
        }
        self.email_values(
            email,
            after,
            maximum,
            (KEYWORD_FIRST, KEYWORD_AFTER),
            |row, retain| {
                let ValueRef::Text(bytes) = row.get_ref(0).map_err(sql)? else {
                    return Err(ports::Error::Corrupt);
                };
                let text = std::str::from_utf8(bytes).map_err(|_| ports::Error::Corrupt)?;
                Key::Keyword(email, text)
                    .validate_local()
                    .map_err(|_| ports::Error::Corrupt)?;
                if !retain {
                    return Ok(None);
                }
                let mut owned = String::new();
                owned
                    .try_reserve_exact(text.len())
                    .map_err(|_| ports::Error::Capacity)?;
                owned.push_str(text);
                Ok(Some(owned))
            },
        )
    }
    fn email_values<T, A: rusqlite::ToSql>(
        &mut self,
        email: EmailId,
        after: Option<A>,
        maximum: usize,
        queries: (&str, &str),
        decode: impl Fn(&rusqlite::Row<'_>, bool) -> Result<Option<T>, ports::Error>,
    ) -> Result<EmailValues<T>, ports::Error> {
        let identity = self.identity;
        self.read_snapshot(|native| {
            native.check()?;
            if maximum == 0 {
                return Err(ports::Error::Invalid);
            }
            if maximum > MAX_EMAIL_VALUES {
                return Err(ports::Error::Capacity);
            }
            let mut values = Vec::new();
            values
                .try_reserve_exact(maximum)
                .map_err(|_| ports::Error::Capacity)?;
            let mut has_more = false;
            native.run(|db| {
                let query = if after.is_some() {
                    queries.1
                } else {
                    queries.0
                };
                let mut statement = db.prepare(query).map_err(sql)?;
                let limit = i64::try_from(maximum + 1).map_err(|_| ports::Error::Capacity)?;
                let mut rows = if let Some(after) = after {
                    statement
                        .query(params![
                            identity.account.as_bytes().as_slice(),
                            email.as_bytes().as_slice(),
                            after,
                            limit
                        ])
                        .map_err(sql)?
                } else {
                    statement
                        .query(params![
                            identity.account.as_bytes().as_slice(),
                            email.as_bytes().as_slice(),
                            limit
                        ])
                        .map_err(sql)?
                };
                while let Some(row) = rows.next().map_err(sql)? {
                    native.check()?;
                    if sequence(&fixed_blob::<8>(row, 1)?)? > identity.committed_sequence {
                        return Err(ports::Error::Corrupt);
                    }
                    let retain = values.len() < maximum;
                    let value = decode(row, retain)?;
                    if !retain {
                        has_more = true;
                        break;
                    }
                    values.push(value.ok_or(ports::Error::Corrupt)?);
                }
                Ok(())
            })?;
            native.check()?;
            Ok(EmailValues {
                identity,
                email,
                values,
                has_more,
            })
        })
    }
}
#[cfg(test)]
#[path = "email_metadata_tests.rs"]
mod tests;
