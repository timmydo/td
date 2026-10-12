//! Bounded received-date windows over one thread in an original snapshot.
use super::*;
use crate::ids::{EmailId, ThreadId};
use std::collections::BinaryHeap;

pub const MAX_THREAD_EMAILS: usize = 256;
/// Passive sort coordinate, not a wire ID or snapshot/authorization proof.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ThreadEmail {
    // Field order is the received-date order, with the ID breaking ties.
    pub received_at: i64,
    pub email: EmailId,
}
/// A bounded ascending window; callers retain and recheck the owning view.
#[derive(Debug)]
pub struct ThreadEmails {
    pub identity: ViewIdentity,
    pub thread: ThreadId,
    pub emails: Vec<ThreadEmail>,
    pub has_more: bool,
}
const MEMBERS: &str = "SELECT id,received_at,changed FROM emails INDEXED BY emails_thread WHERE account=?1 AND thread_id=?2 ORDER BY id";
impl IndexReadView<'_, '_> {
    /// Select at most maximum members by (received_at, id), strictly after the
    /// supplied coordinate. Every window scans this thread's index range.
    /// Thread rows have empty values, so scratch may be empty.
    pub fn thread_emails(
        &mut self,
        thread: ThreadId,
        after: Option<ThreadEmail>,
        maximum: usize,
        scratch: &mut [u8],
    ) -> Result<Option<ThreadEmails>, ports::Error> {
        let identity = self.identity;
        self.read_snapshot(|native| {
            native.check()?;
            if maximum == 0 {
                return Err(ports::Error::Invalid);
            }
            if maximum > MAX_THREAD_EMAILS {
                return Err(ports::Error::Capacity);
            }
            if get(native, identity, Key::Thread(thread), scratch)?.is_none() {
                return Ok(None);
            }
            let mut selected = BinaryHeap::new();
            selected
                .try_reserve_exact(maximum)
                .map_err(|_| ports::Error::Capacity)?;
            let mut more = false;
            native.run(|db| {
                let mut statement = db.prepare(MEMBERS).map_err(sql)?;
                let mut rows = statement
                    .query(params![
                        identity.account.as_bytes().as_slice(),
                        thread.as_bytes().as_slice()
                    ])
                    .map_err(sql)?;
                while let Some(row) = rows.next().map_err(sql)? {
                    let entry = ThreadEmail {
                        email: EmailId::from_bytes(fixed_blob::<16>(row, 0)?),
                        received_at: row.get(1).map_err(sql)?,
                    };
                    if sequence(&fixed_blob::<8>(row, 2)?)? > identity.committed_sequence {
                        return Err(ports::Error::Corrupt);
                    }
                    if after.is_some_and(|bound| entry <= bound) {
                        continue;
                    }
                    if selected.len() < maximum {
                        selected.push(entry);
                    } else {
                        more = true;
                        if let Some(mut last) = selected.peek_mut() {
                            if entry < *last {
                                *last = entry;
                            }
                        }
                    }
                }
                Ok(())
            })?;
            let emails = selected.into_sorted_vec();
            native.check()?;
            Ok(Some(ThreadEmails {
                identity,
                thread,
                emails,
                has_more: more,
            }))
        })
    }
}

#[cfg(test)]
#[path = "thread_emails_tests.rs"]
mod tests;
