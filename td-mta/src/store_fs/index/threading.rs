//! Exact anchor resolution inside one retained account snapshot.
use super::*;
use crate::ids::{EmailId, ThreadId};

const FIRST_ANCHOR: &str = "SELECT email_id,changed FROM thread_anchors WHERE account=?1 AND message_id=?2 ORDER BY email_id LIMIT 1";

impl IndexReadView<'_, '_> {
    /// Resolve an already parsed Message-ID without choosing a new thread.
    /// `format::MAX_VALUE_BYTES` scratch holds every valid Email row.
    /// Returned IDs are passive metadata, not body pins or write authority.
    pub fn thread_anchor(
        &mut self,
        message_id: &str,
        value: &mut [u8],
    ) -> Result<Option<(EmailId, ThreadId)>, ports::Error> {
        let identity = self.identity;
        self.read_snapshot(|native| {
            native.check()?;
            if message_id.is_empty() || message_id.len() > format::key::MAX_ANCHOR_BYTES {
                return Err(ports::Error::Invalid);
            }
            let Some(email) = first_anchor(native, identity, message_id)? else {
                return Ok(None);
            };
            let Some((Row::Email(row), _)) = get(native, identity, Key::Email(email), value)?
            else {
                return Err(ports::Error::Corrupt);
            };
            let thread = row.thread;
            if !matches!(
                get(native, identity, Key::Thread(thread), value)?,
                Some((Row::Thread, _))
            ) {
                return Err(ports::Error::Corrupt);
            }
            Ok(Some((email, thread)))
        })
    }
}

fn first_anchor(
    native: &Native,
    identity: ViewIdentity,
    message_id: &str,
) -> Result<Option<EmailId>, ports::Error> {
    native.run(|db| {
        let mut statement = db.prepare(FIRST_ANCHOR).map_err(sql)?;
        let mut rows = statement
            .query(params![identity.account.as_bytes().as_slice(), message_id])
            .map_err(sql)?;
        let Some(row) = rows.next().map_err(sql)? else {
            return Ok(None);
        };
        let ValueRef::Blob(email) = row.get_ref(0).map_err(sql)? else {
            return Err(ports::Error::Corrupt);
        };
        let email = EmailId::from_bytes(email.try_into().map_err(|_| ports::Error::Corrupt)?);
        let ValueRef::Blob(changed) = row.get_ref(1).map_err(sql)? else {
            return Err(ports::Error::Corrupt);
        };
        if sequence(changed)? > identity.committed_sequence {
            return Err(ports::Error::Corrupt);
        }
        Ok(Some(email))
    })
}

#[cfg(test)]
#[path = "threading_tests.rs"]
mod tests;
