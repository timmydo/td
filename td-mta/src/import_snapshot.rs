//! Canonical source snapshots for insert-only migration receipts.
use crate::{
    format::{
        key::{Key as FormatKey, SourceKind},
        row::{MailboxRow, Row},
    },
    ids::{EmailId, InstanceId},
};
use td_crypto::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Capacity,
    Digest,
}
#[derive(Clone, Eq, PartialEq)]
pub struct Key {
    pub(crate) instance: InstanceId,
    pub(crate) kind: SourceKind,
    pub(crate) account: Vec<u8>,
    pub(crate) object: Vec<u8>,
}
impl Key {
    pub fn new(
        instance: InstanceId,
        kind: SourceKind,
        account: &[u8],
        object: &[u8],
    ) -> Result<Self, Error> {
        FormatKey::Import {
            instance,
            kind,
            account,
            object,
        }
        .validate_local()
        .map_err(|_| Error::Invalid)?;
        Ok(Self {
            instance,
            kind,
            account: account.to_vec(),
            object: object.to_vec(),
        })
    }
}
pub struct MailboxInput<'a> {
    pub name: &'a str,
    pub parent: Option<&'a [u8]>,
    pub role: Option<&'a str>,
    pub sort_order: u32,
    pub subscribed: bool,
}
#[derive(Clone)]
pub struct Mailbox {
    pub(crate) key: Key,
    pub(crate) name: String,
    pub(crate) parent: Option<Vec<u8>>,
    pub(crate) role: Option<String>,
    pub(crate) sort_order: u32,
    pub(crate) subscribed: bool,
    pub(crate) digest: [u8; 32],
}
impl Mailbox {
    pub fn new(key: Key, input: MailboxInput<'_>) -> Result<Self, Error> {
        if key.kind != SourceKind::Mailbox {
            return Err(Error::Invalid);
        }
        Row::Mailbox(MailboxRow {
            name: input.name,
            parent: None,
            role: input.role,
            sort_order: input.sort_order,
            subscribed: input.subscribed,
        })
        .encoded_len()
        .map_err(|_| Error::Invalid)?;
        if let Some(parent) = input.parent {
            if parent.is_empty() || parent.len() > 998 || parent == key.object {
                return Err(Error::Invalid);
            }
        }
        let mut hash = Hash::new()?;
        hash.put(&[1])?;
        hash.bytes(input.name.as_bytes())?;
        hash.optional(input.parent)?;
        hash.optional(input.role.map(str::as_bytes))?;
        hash.put(&input.sort_order.to_le_bytes())?;
        hash.put(&[u8::from(input.subscribed)])?;
        Ok(Self {
            key,
            name: input.name.to_owned(),
            parent: input.parent.map(<[u8]>::to_vec),
            role: input.role.map(str::to_owned),
            sort_order: input.sort_order,
            subscribed: input.subscribed,
            digest: hash.finish()?,
        })
    }
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}
pub struct EmailInput<'a> {
    pub body_digest: [u8; 32],
    pub length: u64,
    pub received_at: i64,
    pub mailboxes: &'a [&'a [u8]],
    pub keywords: &'a [&'a str],
}
#[derive(Clone)]
pub struct Email {
    pub(crate) key: Key,
    pub(crate) body_digest: [u8; 32],
    pub(crate) length: u64,
    pub(crate) received_at: i64,
    pub(crate) mailboxes: Vec<Vec<u8>>,
    pub(crate) keywords: Vec<String>,
    pub(crate) digest: [u8; 32],
}
impl Email {
    pub fn new(key: Key, input: EmailInput<'_>) -> Result<Self, Error> {
        if key.kind != SourceKind::Email
            || input.length > 32 * 1024 * 1024
            || input.mailboxes.is_empty()
            || input.mailboxes.len() > 256
            || input.keywords.len() > 256
        {
            return Err(Error::Capacity);
        }
        let mut mailboxes = Vec::new();
        for id in input.mailboxes {
            if id.is_empty() || id.len() > 998 {
                return Err(Error::Invalid);
            }
            mailboxes.push(id.to_vec());
        }
        mailboxes.sort();
        if mailboxes.windows(2).any(|pair| pair.first() == pair.get(1)) {
            return Err(Error::Invalid);
        }
        let mut keywords = Vec::new();
        for keyword in input.keywords {
            let canonical = keyword.to_ascii_lowercase();
            FormatKey::Keyword(EmailId::from_bytes([0; 16]), &canonical)
                .validate_local()
                .map_err(|_| Error::Invalid)?;
            keywords.push(canonical);
        }
        keywords.sort();
        if keywords.windows(2).any(|pair| pair.first() == pair.get(1)) {
            return Err(Error::Invalid);
        }
        let mut hash = Hash::new()?;
        hash.put(&[3])?;
        hash.put(&input.body_digest)?;
        hash.put(&input.length.to_le_bytes())?;
        hash.put(&input.received_at.to_le_bytes())?;
        hash.put(&(mailboxes.len() as u32).to_le_bytes())?;
        for id in &mailboxes {
            hash.bytes(id)?;
        }
        hash.put(&(keywords.len() as u32).to_le_bytes())?;
        for keyword in &keywords {
            hash.bytes(keyword.as_bytes())?;
        }
        Ok(Self {
            key,
            body_digest: input.body_digest,
            length: input.length,
            received_at: input.received_at,
            mailboxes,
            keywords,
            digest: hash.finish()?,
        })
    }
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}
struct Hash(Sha256);
impl Hash {
    fn new() -> Result<Self, Error> {
        Sha256::try_new().map(Self).map_err(|_| Error::Digest)
    }
    fn put(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.0.update(bytes).map_err(|_| Error::Digest)
    }
    fn bytes(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.put(
            &u32::try_from(bytes.len())
                .map_err(|_| Error::Capacity)?
                .to_le_bytes(),
        )?;
        self.put(bytes)
    }
    fn optional(&mut self, bytes: Option<&[u8]>) -> Result<(), Error> {
        self.put(&[u8::from(bytes.is_some())])?;
        if let Some(bytes) = bytes {
            self.bytes(bytes)?;
        }
        Ok(())
    }
    fn finish(self) -> Result<[u8; 32], Error> {
        self.0.finish().map_err(|_| Error::Digest)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    fn key(kind: SourceKind) -> Key {
        Key::new(InstanceId::from_bytes([1; 16]), kind, b"account", b"object").unwrap()
    }
    fn hex(value: &[u8]) -> String {
        value.iter().map(|b| format!("{b:02x}")).collect()
    }
    #[test]
    fn canonical_email_ignores_order_and_case_but_preserves_metadata() {
        let value = Email::new(
            key(SourceKind::Email),
            EmailInput {
                body_digest: [0x42; 32],
                length: 123,
                received_at: -1,
                mailboxes: &[b"z", b"a"],
                keywords: &["Custom", "$Seen"],
            },
        )
        .unwrap();
        let reordered = Email::new(
            key(SourceKind::Email),
            EmailInput {
                body_digest: [0x42; 32],
                length: 123,
                received_at: -1,
                mailboxes: &[b"a", b"z"],
                keywords: &["$seen", "custom"],
            },
        )
        .unwrap();
        assert_eq!(value.digest(), reordered.digest());
        assert_eq!(
            hex(value.digest()),
            "60cc9ffb3013b2a5bc173f7078e5cb7e03d5e7b158d05c6f198ce2a3f67f2310"
        );
        assert!(Email::new(
            key(SourceKind::Email),
            EmailInput {
                body_digest: [0; 32],
                length: 1,
                received_at: 0,
                mailboxes: &[b"a"],
                keywords: &["Custom", "custom"]
            }
        )
        .is_err());
        assert!(Email::new(
            key(SourceKind::Email),
            EmailInput {
                body_digest: [0; 32],
                length: 1,
                received_at: 0,
                mailboxes: &[b"a", b"a"],
                keywords: &[]
            }
        )
        .is_err());
    }
    #[test]
    fn mailbox_digest_pins_format_little_endian_preimage() {
        let value = Mailbox::new(
            key(SourceKind::Mailbox),
            MailboxInput {
                name: "Folder",
                parent: Some(b"parent"),
                role: None,
                sort_order: 7,
                subscribed: true,
            },
        )
        .unwrap();
        assert_eq!(
            hex(value.digest()),
            "1f5bbda3952ebbfff47341632eda1d1d05374df6696ea5a09a3389413837e3b0"
        );
        assert!(Mailbox::new(
            key(SourceKind::Mailbox),
            MailboxInput {
                name: "Folder",
                parent: Some(b""),
                role: None,
                sort_order: 7,
                subscribed: true
            }
        )
        .is_err());
        assert!(Mailbox::new(
            key(SourceKind::Mailbox),
            MailboxInput {
                name: "Folder",
                parent: None,
                role: Some(""),
                sort_order: 7,
                subscribed: true
            }
        )
        .is_err());
        assert!(Key::new(
            InstanceId::from_bytes([1; 16]),
            SourceKind::Email,
            &[1; 500],
            &[2; 500]
        )
        .is_err());
    }
}
