//! Fixed-memory mailbox totals from one retained account snapshot.
use super::*;
use crate::ids::ThreadId;

pub use crate::mailbox_response::MailboxCounts;

/// Maximum input batch; memory is bounded independently of account size.
pub const MAX_MAILBOX_COUNTS: usize = 256;

// CROSS JOIN fixes the outer scan to thread order without a temporary sort.
const MEMBERS: &str =
    "SELECT m.mailbox_id,e.thread_id,e.changed,t.changed,m.changed,s.changed,d.changed
    FROM emails e INDEXED BY emails_thread
    CROSS JOIN memberships m ON m.account=e.account AND m.email_id=e.id
    LEFT JOIN threads t ON t.account=e.account AND t.id=e.thread_id
    LEFT JOIN keywords s ON s.account=e.account AND s.email_id=e.id AND s.keyword='$seen'
    LEFT JOIN keywords d ON d.account=e.account AND d.email_id=e.id AND d.keyword='$draft'
    WHERE e.account=?1 ORDER BY e.thread_id,e.id";
const MEMBERSHIPS: &str = "SELECT count(*) FROM memberships WHERE account=?1 AND mailbox_id=?2";

struct Slot {
    mailbox: MailboxId,
    ordinal: usize,
    counts: Option<MailboxCounts>,
    previous: Option<ThreadId>,
    thread_unread: bool,
}

impl IndexReadView<'_, '_> {
    /// Count exactly or fail under the retained view's original work/deadline.
    /// Unread threads have an unread, non-draft member in this mailbox.
    /// `format::MAX_VALUE_BYTES` scratch holds every valid Mailbox row.
    pub fn mailbox_counts(
        &mut self,
        mailbox: MailboxId,
        value: &mut [u8],
    ) -> Result<Option<MailboxCounts>, ports::Error> {
        self.mailbox_counts_many(&[mailbox], value)?
            .pop()
            .ok_or(ports::Error::Corrupt)
    }

    /// Count up to MAX_MAILBOX_COUNTS distinct IDs in one account scan.
    /// Return entries in input order; an absent mailbox has a None entry.
    /// Scan cost depends on all account Emails and memberships, not only
    /// selected mailboxes. No result escapes before every count completes.
    pub fn mailbox_counts_many(
        &mut self,
        mailboxes: &[MailboxId],
        value: &mut [u8],
    ) -> Result<Vec<Option<MailboxCounts>>, ports::Error> {
        let identity = self.identity;
        self.read_snapshot(|native| {
            native.check()?;
            if mailboxes.len() > MAX_MAILBOX_COUNTS {
                return Err(ports::Error::Capacity);
            }
            let mut slots = Vec::new();
            slots
                .try_reserve_exact(mailboxes.len())
                .map_err(|_| ports::Error::Capacity)?;
            let mut result = Vec::new();
            result
                .try_reserve_exact(mailboxes.len())
                .map_err(|_| ports::Error::Capacity)?;
            for (ordinal, &mailbox) in mailboxes.iter().enumerate() {
                slots.push(Slot {
                    mailbox,
                    ordinal,
                    counts: None,
                    previous: None,
                    thread_unread: false,
                });
            }
            slots.sort_unstable_by_key(|slot| slot.mailbox);
            if slots.windows(2).any(|pair| {
                pair.first().map(|slot| slot.mailbox) == pair.last().map(|slot| slot.mailbox)
            }) {
                return Err(ports::Error::Invalid);
            }
            for slot in &mut slots {
                if get(native, identity, Key::Mailbox(slot.mailbox), value)?.is_some() {
                    slot.counts = Some(MailboxCounts {
                        identity,
                        mailbox: slot.mailbox,
                        total_emails: 0,
                        unread_emails: 0,
                        total_threads: 0,
                        unread_threads: 0,
                    });
                }
            }
            if slots.iter().any(|slot| slot.counts.is_some()) {
                native.run(|db| read(db, identity, &mut slots))?;
            }
            slots.sort_unstable_by_key(|slot| slot.ordinal);
            result.extend(slots.into_iter().map(|slot| slot.counts));
            Ok(result)
        })
    }
}

fn read(db: &Connection, identity: ViewIdentity, slots: &mut [Slot]) -> Result<(), ports::Error> {
    let account = identity.account.as_bytes().as_slice();
    let mut statement = db.prepare(MEMBERS).map_err(sql)?;
    let mut rows = statement.query(params![account]).map_err(sql)?;
    while let Some(row) = rows.next().map_err(sql)? {
        let ValueRef::Blob(mailbox) = row.get_ref(0).map_err(sql)? else {
            return Err(ports::Error::Corrupt);
        };
        let mailbox = MailboxId::from_bytes(mailbox.try_into().map_err(|_| ports::Error::Corrupt)?);
        let Ok(index) = slots.binary_search_by_key(&mailbox, |slot| slot.mailbox) else {
            continue;
        };
        let slot = slots.get_mut(index).ok_or(ports::Error::Corrupt)?;
        let Some(counts) = &mut slot.counts else {
            continue;
        };
        let ValueRef::Blob(thread) = row.get_ref(1).map_err(sql)? else {
            return Err(ports::Error::Corrupt);
        };
        let thread = ThreadId::from_bytes(thread.try_into().map_err(|_| ports::Error::Corrupt)?);
        for column in 2..=4 {
            if !changed(row.get_ref(column).map_err(sql)?, identity)? {
                return Err(ports::Error::Corrupt);
            }
        }
        let seen = changed(row.get_ref(5).map_err(sql)?, identity)?;
        let draft = changed(row.get_ref(6).map_err(sql)?, identity)?;
        increment(&mut counts.total_emails)?;
        if slot.previous != Some(thread) {
            increment(&mut counts.total_threads)?;
            slot.previous = Some(thread);
            slot.thread_unread = false;
        }
        if !seen && !draft {
            increment(&mut counts.unread_emails)?;
            if !slot.thread_unread {
                increment(&mut counts.unread_threads)?;
                slot.thread_unread = true;
            }
        }
    }
    for slot in slots {
        let Some(counts) = &slot.counts else {
            continue;
        };
        let memberships: i64 = db
            .query_row(
                MEMBERSHIPS,
                params![account, slot.mailbox.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .map_err(sql)?;
        if u64::try_from(memberships).map_err(|_| ports::Error::Corrupt)? != counts.total_emails {
            return Err(ports::Error::Corrupt);
        }
    }
    Ok(())
}

fn changed(value: ValueRef<'_>, identity: ViewIdentity) -> Result<bool, ports::Error> {
    match value {
        ValueRef::Null => Ok(false),
        ValueRef::Blob(bytes) if sequence(bytes)? <= identity.committed_sequence => Ok(true),
        _ => Err(ports::Error::Corrupt),
    }
}

fn increment(value: &mut u64) -> Result<(), ports::Error> {
    *value = value.checked_add(1).ok_or(ports::Error::Corrupt)?;
    Ok(())
}

#[cfg(test)]
#[path = "mailbox_counts_tests.rs"]
mod tests;
