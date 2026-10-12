//! Bounded Mailbox object projection from a retained account snapshot.
use crate::{
    bounded::TextBuffer,
    format::{key::Key, row::Row},
    ids::MailboxId,
    ports::{Error, ReadView},
    store_fs::{IndexReadView, MailboxCounts},
    wire,
};

/// Enough for every valid stored Mailbox object, including JSON escaping.
pub const MAX_OBJECT_BYTES: usize = 8192;

const NAME: u16 = 1;
const PARENT_ID: u16 = 2;
const ROLE: u16 = 4;
const SORT_ORDER: u16 = 8;
const TOTAL_EMAILS: u16 = 16;
const UNREAD_EMAILS: u16 = 32;
const TOTAL_THREADS: u16 = 64;
const UNREAD_THREADS: u16 = 128;
const MY_RIGHTS: u16 = 256;
const IS_SUBSCRIBED: u16 = 512;
const COUNTS: u16 = TOTAL_EMAILS | UNREAD_EMAILS | TOTAL_THREADS | UNREAD_THREADS;
const ALL: u16 = NAME | PARENT_ID | ROLE | SORT_ORDER | COUNTS | MY_RIGHTS | IS_SUBSCRIBED;
const MAX_UNSIGNED_INT: u64 = 9_007_199_254_740_991;

/// Validated property selection; id is always emitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Projection(u16);
impl Projection {
    pub const fn all() -> Self {
        Self(ALL)
    }
    pub const fn id_only() -> Self {
        Self(0)
    }
    pub fn include(&mut self, name: &str) -> Result<(), Error> {
        self.0 |= match name {
            "id" => 0,
            "name" => NAME,
            "parentId" => PARENT_ID,
            "role" => ROLE,
            "sortOrder" => SORT_ORDER,
            "totalEmails" => TOTAL_EMAILS,
            "unreadEmails" => UNREAD_EMAILS,
            "totalThreads" => TOTAL_THREADS,
            "unreadThreads" => UNREAD_THREADS,
            "myRights" => MY_RIGHTS,
            "isSubscribed" => IS_SUBSCRIBED,
            _ => return Err(Error::Invalid),
        };
        Ok(())
    }
    pub const fn needs_counts(self) -> bool {
        self.0 & COUNTS != 0
    }
    fn has(self, bit: u16) -> bool {
        self.0 & bit != 0
    }
}

/// Supplied by trusted account policy; these booleans confer no authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rights {
    pub may_read_items: bool,
    pub may_add_items: bool,
    pub may_remove_items: bool,
    pub may_set_seen: bool,
    pub may_set_keywords: bool,
    pub may_create_child: bool,
    pub may_rename: bool,
    pub may_delete: bool,
    pub may_submit: bool,
}

/// Return a complete object length, or None for an absent mailbox.
/// Caller scratch and output are provisional until this succeeds. Required
/// counts must come from mailbox_counts or mailbox_counts_many on this view;
/// identity checks catch accidental cross-view/account/object substitution.
/// This passive projection does not authorize access or advertise Mail.
pub fn encode(
    view: &mut IndexReadView<'_, '_>,
    id: MailboxId,
    projection: Projection,
    rights: Rights,
    counts: Option<&MailboxCounts>,
    scratch: &mut [u8],
    output: &mut [u8],
) -> Result<Option<usize>, Error> {
    view.check_snapshot()?;
    if let Some(counts) = counts {
        if counts.identity != view.identity() || counts.mailbox != id {
            return Err(Error::Invalid);
        }
    }
    let Some((row, _)) = view.get(Key::Mailbox(id), scratch)? else {
        return Ok(None);
    };
    let Row::Mailbox(row) = row else {
        return Err(Error::Corrupt);
    };
    if projection.needs_counts() && counts.is_none() {
        return Err(Error::Invalid);
    }
    let mut out = TextBuffer::new(output);
    let mut id_bytes = [0; wire::IdKind::WIRE_BYTES];
    let id = wire::encode_id(id, &mut id_bytes).map_err(|_| Error::Invalid)?;
    write(&mut out, format_args!("{{\"id\":\"{id}\""))?;
    if projection.has(NAME) {
        append(&mut out, ",\"name\":")?;
        crate::jmap_session::quoted(&mut out, row.name)?;
    }
    if projection.has(PARENT_ID) {
        append(&mut out, ",\"parentId\":")?;
        if let Some(parent) = row.parent {
            let parent = wire::encode_id(parent, &mut id_bytes).map_err(|_| Error::Invalid)?;
            write(&mut out, format_args!("\"{parent}\""))?;
        } else {
            append(&mut out, "null")?;
        }
    }
    if projection.has(ROLE) {
        append(&mut out, ",\"role\":")?;
        if let Some(role) = row.role {
            crate::jmap_session::quoted(&mut out, role)?;
        } else {
            append(&mut out, "null")?;
        }
    }
    if projection.has(SORT_ORDER) {
        write(&mut out, format_args!(",\"sortOrder\":{}", row.sort_order))?;
    }
    if let Some(counts) = counts {
        for (bit, name, value) in [
            (TOTAL_EMAILS, "totalEmails", counts.total_emails),
            (UNREAD_EMAILS, "unreadEmails", counts.unread_emails),
            (TOTAL_THREADS, "totalThreads", counts.total_threads),
            (UNREAD_THREADS, "unreadThreads", counts.unread_threads),
        ] {
            if projection.has(bit) {
                if value > MAX_UNSIGNED_INT {
                    return Err(Error::Invalid);
                }
                write(&mut out, format_args!(",\"{name}\":{value}"))?;
            }
        }
    }
    if projection.has(MY_RIGHTS) {
        append(&mut out, ",\"myRights\":{")?;
        for (index, (name, value)) in [
            ("mayReadItems", rights.may_read_items),
            ("mayAddItems", rights.may_add_items),
            ("mayRemoveItems", rights.may_remove_items),
            ("maySetSeen", rights.may_set_seen),
            ("maySetKeywords", rights.may_set_keywords),
            ("mayCreateChild", rights.may_create_child),
            ("mayRename", rights.may_rename),
            ("mayDelete", rights.may_delete),
            ("maySubmit", rights.may_submit),
        ]
        .into_iter()
        .enumerate()
        {
            if index != 0 {
                append(&mut out, ",")?;
            }
            write(&mut out, format_args!("\"{name}\":{value}"))?;
        }
        append(&mut out, "}")?;
    }
    if projection.has(IS_SUBSCRIBED) {
        write(
            &mut out,
            format_args!(",\"isSubscribed\":{}", row.subscribed),
        )?;
    }
    append(&mut out, "}")?;
    view.check_snapshot()?;
    Ok(Some(out.len()))
}
fn append(out: &mut TextBuffer<'_>, text: &str) -> Result<(), Error> {
    out.append(text).map_err(|_| Error::Capacity)
}
fn write(out: &mut TextBuffer<'_>, text: std::fmt::Arguments<'_>) -> Result<(), Error> {
    out.format(text).map_err(|_| Error::Capacity)
}

#[cfg(test)]
#[path = "mailbox_response_tests.rs"]
mod tests;
