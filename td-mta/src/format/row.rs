//! Borrowed row values. Row/reference authorization and durable I/O are separate.
use super::{
    scalar::{Reader, Writer},
    Error, Table, MAX_VALUE_BYTES,
};
use crate::ids::{
    AccountId, AttemptId, BlobId, DeviceId, EmailId, IdentityId, MailboxId, ThreadId,
};
use std::net::IpAddr;

macro_rules! tags {
    ($name:ident { $($variant:ident = $tag:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum $name { $($variant),+ }
        impl $name {
            pub const fn tag(self) -> u8 { match self { $(Self::$variant => $tag),+ } }
            pub fn from_tag(tag: u8) -> Result<Self, Error> {
                match tag { $($tag => Ok(Self::$variant),)+ _ => Err(Error::InvalidTag) }
            }
        }
    };
}

tags!(BlobKind { Message = 1, Upload = 2 });
tags!(ReceiptTls { Plain = 0, Tls12 = 1, Tls13 = 2 });
tags!(RecipientState { Queued = 1, InFlight = 2, RetryWait = 3,
    Accepted = 4, Failed = 5, Canceled = 6, OutcomeUnknown = 7 });
tags!(AttemptPhase { None = 0, Prepared = 1, Body = 2,
    AcceptancePossible = 3, Final = 4 });
tags!(NotificationState { None = 0, Pending = 1, Stored = 2 });
tags!(LeaseUse { Import = 1, Attachment = 2, Both = 3 });
tags!(FailureReason { None = 0, SmtpTemporary = 1, SmtpPermanent = 2,
    Network = 3, Tls = 4, Authentication = 5, Expired = 6, Canceled = 7,
    Protocol = 8, Uncertain = 9 });

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlobRow {
    pub kind: BlobKind,
    pub length: u64,
    pub digest: [u8; 32],
    pub created_at: i64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MailboxRow<'a> {
    pub name: &'a str,
    pub parent: Option<MailboxId>,
    pub role: Option<&'a str>,
    pub sort_order: u32,
    pub subscribed: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReceiptRecipients<'a> {
    encoded: &'a [u8],
    count: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SmtpReceipt<'a> {
    pub peer: IpAddr,
    pub gateway: Option<&'a str>,
    pub tls: ReceiptTls,
    pub ehlo: &'a str,
    pub reverse_path: &'a str,
    pub recipients: ReceiptRecipients<'a>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmailOrigin<'a> {
    Smtp(SmtpReceipt<'a>),
    Jmap,
    Import,
    FailureNotice,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmailRow<'a> {
    pub blob: BlobId,
    pub thread: ThreadId,
    pub received_at: i64,
    pub origin: EmailOrigin<'a>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubmissionRow<'a> {
    pub email: EmailId,
    pub thread: ThreadId,
    pub identity: IdentityId,
    pub transmitted_blob: BlobId,
    pub reverse_path: &'a str,
    pub send_at: i64,
    pub expires_at: i64,
    pub recipient_count: u32,
    pub completed_at: Option<i64>,
    pub notification: NotificationState,
    pub notification_email: Option<EmailId>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecipientRow<'a> {
    pub address: &'a str,
    pub state: RecipientState,
    pub uncertain: bool,
    pub attempt: Option<AttemptId>,
    pub attempt_count: u32,
    pub last_attempt_at: Option<i64>,
    pub phase: AttemptPhase,
    pub next_attempt_at: Option<i64>,
    pub rcpt_reply: Option<&'a str>,
    pub data_reply: Option<&'a str>,
    pub reason: FailureReason,
    pub diagnostic: &'a str,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LeaseRow {
    pub account: AccountId,
    pub device: DeviceId,
    pub expires_at: i64,
    pub uses: LeaseUse,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImportRow {
    pub local_object: [u8; 16],
    pub historical_blob: Option<BlobId>,
    pub source_digest: [u8; 32],
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Row<'a> {
    Blob(BlobRow),
    Mailbox(MailboxRow<'a>),
    Email(EmailRow<'a>),
    Membership,
    Keyword,
    Thread,
    ThreadAnchor,
    Submission(SubmissionRow<'a>),
    Recipient(RecipientRow<'a>),
    Lease(LeaseRow),
    Import(ImportRow),
}

pub const MAX_MAILBOX_NAME: usize = 1024;
pub const MAX_ROLE: usize = 64;
pub const MAX_ADDRESS: usize = 254;
pub const MAX_RECIPIENTS: u32 = 1000;
pub const MAX_RECEIPT_BYTES: usize = 32768;
pub const MAX_SMTP_REPLY: usize = 4096;
pub const MAX_DIAGNOSTIC: usize = 512;
pub const MAX_EHLO: usize = 255;
pub const MAX_GATEWAY: usize = 64;
const ORIGIN_SMTP: u8 = 1;
const ORIGIN_JMAP: u8 = 2;
const ORIGIN_IMPORT: u8 = 3;
const ORIGIN_NOTICE: u8 = 4;

/// Common record entry point; live references and authorization remain separate.
pub fn decode_record<'a>(
    table: Table,
    key: &'a [u8],
    value: &'a [u8],
) -> Result<(super::key::Key<'a>, Row<'a>), Error> {
    let key = super::key::Key::decode(table, key)?;
    let row = Row::decode(table, value)?;
    row.validate_key(key)?;
    Ok((key, row))
}

fn text(value: &str, max: usize, nonempty: bool) -> Result<(), Error> {
    if value.len() > max {
        return Err(Error::Limit);
    }
    if (nonempty && value.is_empty()) || value.chars().any(char::is_control) {
        return Err(Error::InvalidValue);
    }
    Ok(())
}
fn address(value: &str, nonempty: bool) -> Result<(), Error> {
    text(value, MAX_ADDRESS, nonempty)?;
    if !value.is_ascii() {
        return Err(Error::InvalidValue);
    }
    Ok(())
}
fn optional<T>(
    reader: &mut Reader<'_>,
    read: impl FnOnce(&mut Reader<'_>) -> Result<T, Error>,
) -> Result<Option<T>, Error> {
    if reader.boolean()? {
        Ok(Some(read(reader)?))
    } else {
        Ok(None)
    }
}
fn opt_text<'a>(reader: &mut Reader<'a>, max: usize) -> Result<Option<&'a str>, Error> {
    if reader.boolean()? {
        Ok(Some(reader.text(max)?))
    } else {
        Ok(None)
    }
}
fn write_option<T>(
    writer: &mut Writer<'_>,
    value: Option<T>,
    write: impl FnOnce(&mut Writer<'_>, T) -> Result<(), Error>,
) -> Result<(), Error> {
    writer.boolean(value.is_some())?;
    if let Some(value) = value {
        write(writer, value)?;
    }
    Ok(())
}
fn put_opt_text(writer: &mut Writer<'_>, value: Option<&str>, max: usize) -> Result<(), Error> {
    write_option(writer, value, |w, v| w.text(v, max))
}

impl<'a> ReceiptRecipients<'a> {
    fn read(reader: &mut Reader<'a>) -> Result<Self, Error> {
        let start = reader.remaining();
        let count = reader.u32()?;
        if count == 0 || count > MAX_RECIPIENTS {
            return Err(Error::Limit);
        }
        for _ in 0..count {
            address(reader.text(MAX_ADDRESS)?, true)?;
        }
        let len = start
            .len()
            .checked_sub(reader.remaining().len())
            .ok_or(Error::Overflow)?;
        if len > MAX_RECEIPT_BYTES {
            return Err(Error::Limit);
        }
        Ok(Self {
            encoded: start.get(..len).ok_or(Error::Truncated)?,
            count,
        })
    }
    pub fn decode(encoded: &'a [u8]) -> Result<Self, Error> {
        if encoded.len() > MAX_RECEIPT_BYTES {
            return Err(Error::Limit);
        }
        let mut reader = Reader::new(encoded);
        let value = Self::read(&mut reader)?;
        reader.finish()?;
        Ok(value)
    }
    /// Validates the entire list and capacity before changing caller storage.
    pub fn encode(addresses: &[&str], output: &'a mut [u8]) -> Result<Self, Error> {
        let count = u32::try_from(addresses.len()).map_err(|_| Error::Limit)?;
        if count == 0 || count > MAX_RECIPIENTS {
            return Err(Error::Limit);
        }
        let mut measure = Writer::measuring();
        measure.u32(count)?;
        for value in addresses {
            address(value, true)?;
            measure.text(value, MAX_ADDRESS)?;
        }
        let len = measure.written();
        if len > MAX_RECEIPT_BYTES {
            return Err(Error::Limit);
        }
        let encoded = output.get_mut(..len).ok_or(Error::OutputFull)?;
        let mut writer = Writer::new(encoded);
        writer.u32(count)?;
        for value in addresses {
            writer.text(value, MAX_ADDRESS)?;
        }
        Ok(Self { encoded, count })
    }
    pub const fn count(self) -> u32 {
        self.count
    }
    pub const fn encoded(self) -> &'a [u8] {
        self.encoded
    }
    pub fn iter(self) -> Result<ReceiptIter<'a>, Error> {
        let mut reader = Reader::new(self.encoded);
        let left = reader.u32()?;
        Ok(ReceiptIter { reader, left })
    }
}
pub struct ReceiptIter<'a> {
    reader: Reader<'a>,
    left: u32,
}
impl<'a> Iterator for ReceiptIter<'a> {
    type Item = Result<&'a str, Error>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let value = self.reader.text(MAX_ADDRESS);
        if value.is_err() {
            self.left = 0;
        }
        Some(value)
    }
}

impl Row<'_> {
    pub const fn table(self) -> Table {
        match self {
            Self::Blob(_) => Table::Blobs,
            Self::Mailbox(_) => Table::Mailboxes,
            Self::Email(_) => Table::Emails,
            Self::Membership => Table::Memberships,
            Self::Keyword => Table::Keywords,
            Self::Thread => Table::Threads,
            Self::ThreadAnchor => Table::ThreadAnchors,
            Self::Submission(_) => Table::Submissions,
            Self::Recipient(_) => Table::Recipients,
            Self::Lease(_) => Table::Leases,
            Self::Import(_) => Table::Imports,
        }
    }
    /// Checks local key/value rules, not live references or authorization.
    pub fn validate_key(self, key: super::key::Key<'_>) -> Result<(), Error> {
        use super::key::{Key, SourceKind};
        key.encoded_len()?;
        self.encoded_len()?;
        if key.table() != self.table() {
            return Err(Error::InvalidValue);
        }
        match (key, self) {
            (Key::Keyword(_, keyword), Self::Keyword) => {
                if !keyword.bytes().all(|b| {
                    (0x21..=0x7e).contains(&b)
                        && !b.is_ascii_uppercase()
                        && !matches!(b, b'(' | b')' | b'{' | b']' | b'%' | b'*' | b'"' | b'\\')
                }) {
                    return Err(Error::InvalidValue);
                }
            }
            (Key::Import { kind, .. }, Self::Import(row)) => {
                if (kind == SourceKind::Email) != row.historical_blob.is_some() {
                    return Err(Error::InvalidValue);
                }
            }
            (Key::Mailbox(id), Self::Mailbox(row)) if row.parent == Some(id) => {
                return Err(Error::InvalidValue)
            }
            (Key::Recipient(_, ordinal), _) if ordinal >= MAX_RECIPIENTS => {
                return Err(Error::Limit)
            }
            _ => {}
        }
        Ok(())
    }
    pub fn encoded_len(self) -> Result<usize, Error> {
        let mut writer = Writer::measuring();
        self.write_fields(&mut writer)?;
        if writer.written() > MAX_VALUE_BYTES {
            return Err(Error::Limit);
        }
        Ok(writer.written())
    }
    /// Like key encoding, leaves the whole output unchanged on any error.
    pub fn encode(self, output: &mut [u8]) -> Result<usize, Error> {
        let len = self.encoded_len()?;
        let bytes = output.get_mut(..len).ok_or(Error::OutputFull)?;
        self.write_fields(&mut Writer::new(bytes))?;
        Ok(len)
    }
    fn write_fields(self, w: &mut Writer<'_>) -> Result<(), Error> {
        match self {
            Self::Blob(v) => {
                w.u8(v.kind.tag())?;
                w.u64(v.length)?;
                w.put(&v.digest)?;
                w.i64(v.created_at)?;
            }
            Self::Mailbox(v) => {
                text(v.name, MAX_MAILBOX_NAME, true)?;
                if v.sort_order >= 1 << 31 {
                    return Err(Error::InvalidValue);
                }
                if let Some(role) = v.role {
                    text(role, MAX_ROLE, true)?;
                    if !role
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    {
                        return Err(Error::InvalidValue);
                    }
                }
                w.text(v.name, MAX_MAILBOX_NAME)?;
                write_option(w, v.parent, |w, id| w.put(id.as_bytes()))?;
                put_opt_text(w, v.role, MAX_ROLE)?;
                w.u32(v.sort_order)?;
                w.boolean(v.subscribed)?;
            }
            Self::Email(v) => {
                w.put(v.blob.as_bytes())?;
                w.put(v.thread.as_bytes())?;
                w.i64(v.received_at)?;
                match v.origin {
                    EmailOrigin::Smtp(receipt) => {
                        text(receipt.ehlo, MAX_EHLO, true)?;
                        address(receipt.reverse_path, false)?;
                        if let Some(gateway) = receipt.gateway {
                            text(gateway, MAX_GATEWAY, true)?;
                        }
                        w.u8(ORIGIN_SMTP)?;
                        match receipt.peer {
                            IpAddr::V4(ip) => {
                                w.u8(4)?;
                                w.put(&ip.octets())?;
                            }
                            IpAddr::V6(ip) => {
                                w.u8(6)?;
                                w.put(&ip.octets())?;
                            }
                        }
                        put_opt_text(w, receipt.gateway, MAX_GATEWAY)?;
                        w.u8(receipt.tls.tag())?;
                        w.text(receipt.ehlo, MAX_EHLO)?;
                        w.text(receipt.reverse_path, MAX_ADDRESS)?;
                        w.put(receipt.recipients.encoded)?;
                    }
                    EmailOrigin::Jmap => w.u8(ORIGIN_JMAP)?,
                    EmailOrigin::Import => w.u8(ORIGIN_IMPORT)?,
                    EmailOrigin::FailureNotice => w.u8(ORIGIN_NOTICE)?,
                }
            }
            Self::Membership | Self::Keyword | Self::Thread | Self::ThreadAnchor => {}
            Self::Submission(v) => {
                address(v.reverse_path, false)?;
                if v.recipient_count == 0 || v.recipient_count > MAX_RECIPIENTS {
                    return Err(Error::Limit);
                }
                if v.expires_at < v.send_at {
                    return Err(Error::InvalidValue);
                }
                if (v.notification == NotificationState::Stored) != v.notification_email.is_some() {
                    return Err(Error::InvalidValue);
                }
                w.put(v.email.as_bytes())?;
                w.put(v.thread.as_bytes())?;
                w.put(v.identity.as_bytes())?;
                w.put(v.transmitted_blob.as_bytes())?;
                w.text(v.reverse_path, MAX_ADDRESS)?;
                w.i64(v.send_at)?;
                w.i64(v.expires_at)?;
                w.u32(v.recipient_count)?;
                write_option(w, v.completed_at, |w, v| w.i64(v))?;
                w.u8(v.notification.tag())?;
                write_option(w, v.notification_email, |w, id| w.put(id.as_bytes()))?;
            }
            Self::Recipient(v) => {
                address(v.address, true)?;
                for reply in [v.rcpt_reply, v.data_reply].into_iter().flatten() {
                    text(reply, MAX_SMTP_REPLY, true)?;
                }
                text(v.diagnostic, MAX_DIAGNOSTIC, false)?;
                let attempted = v.attempt_count != 0;
                if v.attempt.is_some() != attempted
                    || v.last_attempt_at.is_some() != attempted
                    || (v.phase != AttemptPhase::None) != attempted
                {
                    return Err(Error::InvalidValue);
                }
                if v.state == RecipientState::OutcomeUnknown && !v.uncertain {
                    return Err(Error::InvalidValue);
                }
                if v.state == RecipientState::Canceled && v.uncertain {
                    return Err(Error::InvalidValue);
                }
                w.text(v.address, MAX_ADDRESS)?;
                w.u8(v.state.tag())?;
                w.boolean(v.uncertain)?;
                write_option(w, v.attempt, |w, id| w.put(id.as_bytes()))?;
                w.u32(v.attempt_count)?;
                write_option(w, v.last_attempt_at, |w, v| w.i64(v))?;
                w.u8(v.phase.tag())?;
                write_option(w, v.next_attempt_at, |w, v| w.i64(v))?;
                put_opt_text(w, v.rcpt_reply, MAX_SMTP_REPLY)?;
                put_opt_text(w, v.data_reply, MAX_SMTP_REPLY)?;
                w.u8(v.reason.tag())?;
                w.text(v.diagnostic, MAX_DIAGNOSTIC)?;
            }
            Self::Lease(v) => {
                w.put(v.account.as_bytes())?;
                w.put(v.device.as_bytes())?;
                w.i64(v.expires_at)?;
                w.u8(v.uses.tag())?;
            }
            Self::Import(v) => {
                w.put(&v.local_object)?;
                write_option(w, v.historical_blob, |w, id| w.put(id.as_bytes()))?;
                w.put(&v.source_digest)?;
            }
        }
        Ok(())
    }
}

impl<'a> Row<'a> {
    pub fn decode(table: Table, bytes: &'a [u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_VALUE_BYTES {
            return Err(Error::Limit);
        }
        let mut r = Reader::new(bytes);
        let value = match table {
            Table::Blobs => Self::Blob(BlobRow {
                kind: BlobKind::from_tag(r.u8()?)?,
                length: r.u64()?,
                digest: r.fixed()?,
                created_at: r.i64()?,
            }),
            Table::Mailboxes => Self::Mailbox(MailboxRow {
                name: r.text(MAX_MAILBOX_NAME)?,
                parent: optional(&mut r, |r| Ok(MailboxId::from_bytes(r.fixed()?)))?,
                role: opt_text(&mut r, MAX_ROLE)?,
                sort_order: r.u32()?,
                subscribed: r.boolean()?,
            }),
            Table::Emails => {
                let blob = BlobId::from_bytes(r.fixed()?);
                let thread = ThreadId::from_bytes(r.fixed()?);
                let received_at = r.i64()?;
                let origin = match r.u8()? {
                    ORIGIN_SMTP => EmailOrigin::Smtp(SmtpReceipt {
                        peer: match r.u8()? {
                            4 => IpAddr::from(r.fixed::<4>()?),
                            6 => IpAddr::from(r.fixed::<16>()?),
                            _ => return Err(Error::InvalidTag),
                        },
                        gateway: opt_text(&mut r, MAX_GATEWAY)?,
                        tls: ReceiptTls::from_tag(r.u8()?)?,
                        ehlo: r.text(MAX_EHLO)?,
                        reverse_path: r.text(MAX_ADDRESS)?,
                        recipients: ReceiptRecipients::read(&mut r)?,
                    }),
                    ORIGIN_JMAP => EmailOrigin::Jmap,
                    ORIGIN_IMPORT => EmailOrigin::Import,
                    ORIGIN_NOTICE => EmailOrigin::FailureNotice,
                    _ => return Err(Error::InvalidTag),
                };
                Self::Email(EmailRow {
                    blob,
                    thread,
                    received_at,
                    origin,
                })
            }
            Table::Memberships => Self::Membership,
            Table::Keywords => Self::Keyword,
            Table::Threads => Self::Thread,
            Table::ThreadAnchors => Self::ThreadAnchor,
            Table::Submissions => Self::Submission(SubmissionRow {
                email: EmailId::from_bytes(r.fixed()?),
                thread: ThreadId::from_bytes(r.fixed()?),
                identity: IdentityId::from_bytes(r.fixed()?),
                transmitted_blob: BlobId::from_bytes(r.fixed()?),
                reverse_path: r.text(MAX_ADDRESS)?,
                send_at: r.i64()?,
                expires_at: r.i64()?,
                recipient_count: r.u32()?,
                completed_at: optional(&mut r, |r| r.i64())?,
                notification: NotificationState::from_tag(r.u8()?)?,
                notification_email: optional(&mut r, |r| Ok(EmailId::from_bytes(r.fixed()?)))?,
            }),
            Table::Recipients => Self::Recipient(RecipientRow {
                address: r.text(MAX_ADDRESS)?,
                state: RecipientState::from_tag(r.u8()?)?,
                uncertain: r.boolean()?,
                attempt: optional(&mut r, |r| Ok(AttemptId::from_bytes(r.fixed()?)))?,
                attempt_count: r.u32()?,
                last_attempt_at: optional(&mut r, |r| r.i64())?,
                phase: AttemptPhase::from_tag(r.u8()?)?,
                next_attempt_at: optional(&mut r, |r| r.i64())?,
                rcpt_reply: opt_text(&mut r, MAX_SMTP_REPLY)?,
                data_reply: opt_text(&mut r, MAX_SMTP_REPLY)?,
                reason: FailureReason::from_tag(r.u8()?)?,
                diagnostic: r.text(MAX_DIAGNOSTIC)?,
            }),
            Table::Leases => Self::Lease(LeaseRow {
                account: AccountId::from_bytes(r.fixed()?),
                device: DeviceId::from_bytes(r.fixed()?),
                expires_at: r.i64()?,
                uses: LeaseUse::from_tag(r.u8()?)?,
            }),
            Table::Imports => Self::Import(ImportRow {
                local_object: r.fixed()?,
                historical_blob: optional(&mut r, |r| Ok(BlobId::from_bytes(r.fixed()?)))?,
                source_digest: r.fixed()?,
            }),
        };
        r.finish()?;
        value.encoded_len()?;
        Ok(value)
    }
}
