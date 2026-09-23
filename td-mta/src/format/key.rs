//! Canonical primary keys. Semantic row/reference validation belongs to the store.
use super::{
    scalar::{Reader, Writer},
    Error, ObjectType, Table, MAX_KEY_BYTES,
};
use crate::ids::{BlobId, EmailId, InstanceId, MailboxId, SubmissionId, ThreadId};

pub const MAX_KEYWORD_BYTES: usize = 255;
pub const MAX_ANCHOR_BYTES: usize = 1004;
pub const MAX_SOURCE_IDS_BYTES: usize = 999;
pub const MAX_SOURCE_ID_BYTES: usize = MAX_SOURCE_IDS_BYTES - 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceKind {
    Mailbox,
    Email,
}

impl SourceKind {
    pub const fn tag(self) -> u8 {
        match self {
            Self::Mailbox => ObjectType::Mailbox.tag(),
            Self::Email => ObjectType::Email.tag(),
        }
    }
    pub fn from_tag(tag: u8) -> Result<Self, Error> {
        match ObjectType::from_tag(u16::from(tag))? {
            ObjectType::Mailbox => Ok(Self::Mailbox),
            ObjectType::Email => Ok(Self::Email),
            _ => Err(Error::InvalidTag),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key<'a> {
    Blob(BlobId),
    Mailbox(MailboxId),
    Email(EmailId),
    Membership(EmailId, MailboxId),
    Keyword(EmailId, &'a str),
    Thread(ThreadId),
    ThreadAnchor(&'a str, EmailId),
    Submission(SubmissionId),
    Recipient(SubmissionId, u32),
    Lease(BlobId),
    Import {
        instance: InstanceId,
        kind: SourceKind,
        account: &'a [u8],
        object: &'a [u8],
    },
}

fn bounded_nonempty(bytes: &[u8], max: usize) -> Result<usize, Error> {
    if bytes.is_empty() || bytes.len() > max {
        Err(Error::Limit)
    } else {
        Ok(bytes.len())
    }
}

impl<'a> Key<'a> {
    pub const fn table(self) -> Table {
        match self {
            Self::Blob(_) => Table::Blobs,
            Self::Mailbox(_) => Table::Mailboxes,
            Self::Email(_) => Table::Emails,
            Self::Membership(..) => Table::Memberships,
            Self::Keyword(..) => Table::Keywords,
            Self::Thread(_) => Table::Threads,
            Self::ThreadAnchor(..) => Table::ThreadAnchors,
            Self::Submission(_) => Table::Submissions,
            Self::Recipient(..) => Table::Recipients,
            Self::Lease(_) => Table::Leases,
            Self::Import { .. } => Table::Imports,
        }
    }

    /// Checks the complete key; individual import-ID bounds alone are insufficient.
    pub fn encoded_len(self) -> Result<usize, Error> {
        let len = match self {
            Self::Membership(..) => Ok(32),
            Self::Recipient(..) => Ok(20),
            Self::Keyword(_, text) => bounded_nonempty(text.as_bytes(), MAX_KEYWORD_BYTES)?
                .checked_add(16)
                .ok_or(Error::Overflow),
            Self::ThreadAnchor(text, _) => bounded_nonempty(text.as_bytes(), MAX_ANCHOR_BYTES)?
                .checked_add(20)
                .ok_or(Error::Overflow),
            Self::Import {
                account, object, ..
            } => {
                let account_len = bounded_nonempty(account, MAX_SOURCE_ID_BYTES)?;
                let object_len = bounded_nonempty(object, MAX_SOURCE_ID_BYTES)?;
                account_len
                    .checked_add(object_len)
                    .and_then(|n| n.checked_add(25))
                    .ok_or(Error::Overflow)
            }
            _ => Ok(16),
        }?;
        if len > MAX_KEY_BYTES {
            Err(Error::Limit)
        } else {
            Ok(len)
        }
    }

    /// Returns the occupied prefix length; caller output is unchanged on error.
    pub fn encode(self, output: &mut [u8]) -> Result<usize, Error> {
        let len = self.encoded_len()?;
        let mut writer = Writer::new(output.get_mut(..len).ok_or(Error::OutputFull)?);
        match self {
            Self::Blob(id) | Self::Lease(id) => writer.put(id.as_bytes())?,
            Self::Mailbox(id) => writer.put(id.as_bytes())?,
            Self::Email(id) => writer.put(id.as_bytes())?,
            Self::Thread(id) => writer.put(id.as_bytes())?,
            Self::Submission(id) => writer.put(id.as_bytes())?,
            Self::Membership(email, mailbox) => {
                writer.put(email.as_bytes())?;
                writer.put(mailbox.as_bytes())?;
            }
            Self::Keyword(email, text) => {
                writer.put(email.as_bytes())?;
                writer.put(text.as_bytes())?;
            }
            Self::ThreadAnchor(text, email) => {
                writer.text(text, MAX_ANCHOR_BYTES)?;
                writer.put(email.as_bytes())?;
            }
            Self::Recipient(submission, ordinal) => {
                writer.put(submission.as_bytes())?;
                writer.u32_key(ordinal)?;
            }
            Self::Import {
                instance,
                kind,
                account,
                object,
            } => {
                writer.put(instance.as_bytes())?;
                writer.u8(kind.tag())?;
                writer.bytes(account, MAX_SOURCE_ID_BYTES)?;
                writer.bytes(object, MAX_SOURCE_ID_BYTES)?;
            }
        }
        Ok(writer.written())
    }

    pub fn decode(table: Table, bytes: &'a [u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_KEY_BYTES {
            return Err(Error::Limit);
        }
        let mut reader = Reader::new(bytes);
        let key = match table {
            Table::Blobs => Self::Blob(BlobId::from_bytes(reader.fixed()?)),
            Table::Mailboxes => Self::Mailbox(MailboxId::from_bytes(reader.fixed()?)),
            Table::Emails => Self::Email(EmailId::from_bytes(reader.fixed()?)),
            Table::Memberships => Self::Membership(
                EmailId::from_bytes(reader.fixed()?),
                MailboxId::from_bytes(reader.fixed()?),
            ),
            Table::Keywords => {
                let email = EmailId::from_bytes(reader.fixed()?);
                let len = bytes.len().checked_sub(16).ok_or(Error::Truncated)?;
                let text =
                    std::str::from_utf8(reader.take(len)?).map_err(|_| Error::InvalidUtf8)?;
                Self::Keyword(email, text)
            }
            Table::Threads => Self::Thread(ThreadId::from_bytes(reader.fixed()?)),
            Table::ThreadAnchors => Self::ThreadAnchor(
                reader.text(MAX_ANCHOR_BYTES)?,
                EmailId::from_bytes(reader.fixed()?),
            ),
            Table::Submissions => Self::Submission(SubmissionId::from_bytes(reader.fixed()?)),
            Table::Recipients => {
                Self::Recipient(SubmissionId::from_bytes(reader.fixed()?), reader.u32_key()?)
            }
            Table::Leases => Self::Lease(BlobId::from_bytes(reader.fixed()?)),
            Table::Imports => Self::Import {
                instance: InstanceId::from_bytes(reader.fixed()?),
                kind: SourceKind::from_tag(reader.u8()?)?,
                account: reader.bytes(MAX_SOURCE_ID_BYTES)?,
                object: reader.bytes(MAX_SOURCE_ID_BYTES)?,
            },
        };
        reader.finish()?;
        key.encoded_len()?;
        Ok(key)
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn all_table_keys_have_canonical_encodings() -> Result<(), Error> {
        let a = [0x11; 16];
        let b = [0x22; 16];
        let email = EmailId::from_bytes(a);
        let mailbox = MailboxId::from_bytes(b);
        let submission = SubmissionId::from_bytes(a);
        let examples = [
            (Key::Blob(BlobId::from_bytes(a)), a.to_vec()),
            (Key::Mailbox(mailbox), b.to_vec()),
            (Key::Email(email), a.to_vec()),
            (
                Key::Membership(email, mailbox),
                [a.as_slice(), b.as_slice()].concat(),
            ),
            (
                Key::Keyword(email, "$seen"),
                [a.as_slice(), b"$seen"].concat(),
            ),
            (Key::Thread(ThreadId::from_bytes(a)), a.to_vec()),
            (
                Key::ThreadAnchor("a@b", email),
                [&[3, 0, 0, 0], b"a@b".as_slice(), a.as_slice()].concat(),
            ),
            (Key::Submission(submission), a.to_vec()),
            (
                Key::Recipient(submission, 258),
                [a.as_slice(), &[0, 0, 1, 2]].concat(),
            ),
            (Key::Lease(BlobId::from_bytes(a)), a.to_vec()),
            (
                Key::Import {
                    instance: InstanceId::from_bytes(a),
                    kind: SourceKind::Email,
                    account: b"A",
                    object: b"E",
                },
                [a.as_slice(), &[3, 1, 0, 0, 0, b'A', 1, 0, 0, 0, b'E']].concat(),
            ),
        ];
        for (key, golden) in examples {
            let mut output = [0xaa; MAX_KEY_BYTES];
            let used = key.encode(&mut output)?;
            assert_eq!(&output[..used], golden);
            assert_eq!(Key::decode(key.table(), &golden)?, key);
            for len in 0..golden.len() {
                // Keywords have a trailing unprefixed string: a shorter nonempty
                // keyword is another valid key, not a truncation of its framing.
                if key.table() != Table::Keywords || len <= 16 {
                    assert!(Key::decode(key.table(), &golden[..len]).is_err());
                }
                let mut short = vec![0xaa; len];
                assert_eq!(key.encode(&mut short), Err(Error::OutputFull));
                assert!(short.iter().all(|byte| *byte == 0xaa));
            }
        }
        Ok(())
    }

    #[test]
    fn import_namespaces_and_recipient_order_are_distinct() -> Result<(), Error> {
        let base = Key::Import {
            instance: InstanceId::from_bytes([1; 16]),
            kind: SourceKind::Email,
            account: b"a",
            object: b"same",
        };
        let other = Key::Import {
            instance: InstanceId::from_bytes([1; 16]),
            kind: SourceKind::Mailbox,
            account: b"a",
            object: b"same",
        };
        let mut first = [0; 64];
        let mut second = [0; 64];
        assert_eq!(base.encode(&mut first)?, other.encode(&mut second)?);
        assert_ne!(first, second);
        let submission = SubmissionId::from_bytes([0; 16]);
        Key::Recipient(submission, 255).encode(&mut first)?;
        Key::Recipient(submission, 256).encode(&mut second)?;
        assert!(first[..20] < second[..20]);
        Ok(())
    }

    #[test]
    fn maximum_keys_and_invalid_inputs() -> Result<(), Error> {
        let mut output = [0; MAX_KEY_BYTES];
        let email = EmailId::from_bytes([0; 16]);
        let keyword = "k".repeat(MAX_KEYWORD_BYTES);
        let key = Key::Keyword(email, &keyword);
        let used = key.encode(&mut output)?;
        assert_eq!(used, 271);
        assert_eq!(Key::decode(Table::Keywords, &output[..used])?, key);
        let oversized_keyword = "k".repeat(MAX_KEYWORD_BYTES + 1);
        assert_eq!(
            Key::Keyword(email, &oversized_keyword).encode(&mut output),
            Err(Error::Limit)
        );
        let key = Key::ThreadAnchor("é@example.test", email);
        let used = key.encode(&mut output)?;
        assert_eq!(Key::decode(Table::ThreadAnchors, &output[..used])?, key);
        let anchor = "x".repeat(MAX_ANCHOR_BYTES);
        assert_eq!(
            Key::ThreadAnchor(&anchor, EmailId::from_bytes([0; 16])).encode(&mut output)?,
            MAX_KEY_BYTES
        );
        let source = [b'x'; 998];
        let key = Key::Import {
            instance: InstanceId::from_bytes([1; 16]),
            kind: SourceKind::Email,
            account: b"a",
            object: &source,
        };
        assert_eq!(key.encode(&mut output)?, MAX_KEY_BYTES);
        assert_eq!(Key::decode(Table::Imports, &output)?, key);
        let too_long = [b'x'; 999];
        let key = Key::Import {
            instance: InstanceId::from_bytes([1; 16]),
            kind: SourceKind::Email,
            account: b"a",
            object: &too_long,
        };
        assert_eq!(key.encode(&mut output), Err(Error::Limit));
        output[16] = 2;
        assert_eq!(Key::decode(Table::Imports, &output), Err(Error::InvalidTag));
        assert_eq!(
            Key::decode(Table::Blobs, &[0; 17]),
            Err(Error::TrailingBytes)
        );
        assert_eq!(Key::decode(Table::Keywords, &[0; 16]), Err(Error::Limit));
        let mut invalid = [0; 17];
        invalid[16] = 0xff;
        assert_eq!(
            Key::decode(Table::Keywords, &invalid),
            Err(Error::InvalidUtf8)
        );
        assert_eq!(
            Key::decode(Table::ThreadAnchors, &[0xff; 4]),
            Err(Error::Limit)
        );
        assert_eq!(Key::decode(Table::Imports, &[0; 1025]), Err(Error::Limit));
        Ok(())
    }

    #[test]
    fn empty_key_fields_refuse_in_both_directions() {
        let email = EmailId::from_bytes([0; 16]);
        let instance = InstanceId::from_bytes([0; 16]);
        let cases = [
            (Key::Keyword(email, ""), vec![0; 16]),
            (Key::ThreadAnchor("", email), vec![0; 20]),
            (
                Key::Import {
                    instance,
                    kind: SourceKind::Email,
                    account: b"",
                    object: b"a",
                },
                [&[0; 16][..], &[3, 0, 0, 0, 0, 1, 0, 0, 0, b'a']].concat(),
            ),
            (
                Key::Import {
                    instance,
                    kind: SourceKind::Email,
                    account: b"a",
                    object: b"",
                },
                [&[0; 16][..], &[3, 1, 0, 0, 0, b'a', 0, 0, 0, 0]].concat(),
            ),
        ];
        for (key, bytes) in cases {
            let mut output = [0xaa; MAX_KEY_BYTES];
            assert_eq!(key.encode(&mut output), Err(Error::Limit));
            assert!(output.iter().all(|byte| *byte == 0xaa));
            assert_eq!(Key::decode(key.table(), &bytes), Err(Error::Limit));
        }
    }
}
