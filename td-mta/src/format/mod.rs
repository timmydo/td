//! Format-v1 scalar/key/row contracts. These codecs do not validate store integrity.
//! FORMAT.md owns the byte layout; persistence and digest checks follow in M05.
use std::fmt;

pub mod key;
pub mod row;
pub mod scalar;

pub const CONTAINER_VERSION: u16 = 1;
pub const SCHEMA_VERSION: u16 = 1;
pub const MAX_KEY_BYTES: usize = 1024;
pub const MIN_KEY_BYTES: usize = 16;
pub const MAX_VALUE_BYTES: usize = 65536;
pub const FORMAT_BYTES: usize = 80;
pub const CURRENT_BYTES: usize = 120;
pub const TABLE_HEADER_BYTES: usize = 112;
pub const JOURNAL_HEADER_BYTES: usize = 96;
pub const FRAME_HEADER_BYTES: usize = 64;
pub const FRAME_FOOTER_BYTES: usize = 40;
pub const RECORD_OVERHEAD_BYTES: usize = 48;
pub const MANIFEST_PREFIX_BYTES: usize = 88;
pub const TABLE_DESCRIPTOR_BYTES: usize = 56;
pub const HISTORY_DESCRIPTOR_BYTES: usize = 64;
pub const MANIFEST_DIGEST_BYTES: usize = 32;
pub const TABLE_COUNT: usize = 11;
pub const MAX_HISTORY_DESCRIPTORS: usize = 64;
pub const MAX_MANIFEST_BYTES: usize = 4832;
pub const OPERATION_HEADER_BYTES: usize = 12;
pub const MIN_FRAME_BYTES: usize =
    FRAME_HEADER_BYTES + FRAME_FOOTER_BYTES + OPERATION_HEADER_BYTES + MIN_KEY_BYTES;
pub const MAX_FRAME_BYTES: usize = 1_048_576;
pub const MAX_FRAME_OPERATIONS: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Truncated,
    OutputFull,
    TrailingBytes,
    InvalidTag,
    InvalidValue,
    InvalidUtf8,
    Limit,
    Overflow,
    Exhausted,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Truncated => "incomplete format value",
            Self::OutputFull => "format output buffer too small",
            Self::TrailingBytes => "trailing format bytes",
            Self::InvalidTag => "invalid format tag or identifier",
            Self::InvalidValue => "inconsistent or invalid format value",
            Self::InvalidUtf8 => "invalid format UTF-8",
            Self::Limit => "format field exceeds its bound",
            Self::Overflow => "format length arithmetic overflow",
            Self::Exhausted => "format sequence exhausted",
        })
    }
}

impl std::error::Error for Error {}

/// Zero describes an empty initial checkpoint; transaction sequences start at 1.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Sequence(u64);

impl Sequence {
    pub const fn from_u64(value: u64) -> Self {
        Self(value)
    }
    pub const fn number(self) -> u64 {
        self.0
    }
    pub fn successor(self) -> Result<Self, Error> {
        self.0.checked_add(1).map(Self).ok_or(Error::Exhausted)
    }
}

/// Numeric table tags are permanent; Rust enum layout is never serialized.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Table {
    Blobs,
    Mailboxes,
    Emails,
    Memberships,
    Keywords,
    Threads,
    ThreadAnchors,
    Submissions,
    Recipients,
    Leases,
    Imports,
}

/// Shared by import key kinds and journal CHANGE records; not Rust discriminants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectType {
    Mailbox,
    Thread,
    Email,
    Identity,
    EmailSubmission,
}

impl ObjectType {
    pub const fn tag(self) -> u8 {
        match self {
            Self::Mailbox => 1,
            Self::Thread => 2,
            Self::Email => 3,
            Self::Identity => 4,
            Self::EmailSubmission => 5,
        }
    }
    pub fn from_tag(tag: u16) -> Result<Self, Error> {
        match tag {
            1 => Ok(Self::Mailbox),
            2 => Ok(Self::Thread),
            3 => Ok(Self::Email),
            4 => Ok(Self::Identity),
            5 => Ok(Self::EmailSubmission),
            _ => Err(Error::InvalidTag),
        }
    }
}

impl Table {
    pub const fn tag(self) -> u16 {
        match self {
            Self::Blobs => 1,
            Self::Mailboxes => 2,
            Self::Emails => 3,
            Self::Memberships => 4,
            Self::Keywords => 5,
            Self::Threads => 6,
            Self::ThreadAnchors => 7,
            Self::Submissions => 8,
            Self::Recipients => 9,
            Self::Leases => 10,
            Self::Imports => 11,
        }
    }
    pub fn from_tag(tag: u16) -> Result<Self, Error> {
        match tag {
            1 => Ok(Self::Blobs),
            2 => Ok(Self::Mailboxes),
            3 => Ok(Self::Emails),
            4 => Ok(Self::Memberships),
            5 => Ok(Self::Keywords),
            6 => Ok(Self::Threads),
            7 => Ok(Self::ThreadAnchors),
            8 => Ok(Self::Submissions),
            9 => Ok(Self::Recipients),
            10 => Ok(Self::Leases),
            11 => Ok(Self::Imports),
            _ => Err(Error::InvalidTag),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_never_wraps_or_reuses_zero() -> Result<(), Error> {
        assert_eq!(Sequence::default().successor()?.number(), 1);
        assert_eq!(
            Sequence::from_u64(u64::MAX).successor(),
            Err(Error::Exhausted)
        );
        Ok(())
    }

    #[test]
    fn unknown_table_tags_fail_closed() {
        assert_eq!(Table::from_tag(0), Err(Error::InvalidTag));
        assert_eq!(Table::from_tag(12), Err(Error::InvalidTag));
        assert_eq!(Table::from_tag(u16::MAX), Err(Error::InvalidTag));
    }

    #[test]
    fn table_numbers_are_stable_wire_values() -> Result<(), Error> {
        for (tag, table) in [
            (1, Table::Blobs),
            (2, Table::Mailboxes),
            (3, Table::Emails),
            (4, Table::Memberships),
            (5, Table::Keywords),
            (6, Table::Threads),
            (7, Table::ThreadAnchors),
            (8, Table::Submissions),
            (9, Table::Recipients),
            (10, Table::Leases),
            (11, Table::Imports),
        ] {
            assert_eq!(table.tag(), tag);
            assert_eq!(Table::from_tag(tag)?, table);
        }
        Ok(())
    }

    #[test]
    fn registry_matches_the_admission_profile() {
        let limits = crate::limits::Limits::default();
        assert_eq!(limits.frame_bytes, MAX_FRAME_BYTES);
        assert_eq!(limits.frame_operations, MAX_FRAME_OPERATIONS);
        assert_eq!(MIN_FRAME_BYTES, 132);
        assert_eq!(
            MAX_MANIFEST_BYTES,
            MANIFEST_PREFIX_BYTES
                + TABLE_COUNT * TABLE_DESCRIPTOR_BYTES
                + MAX_HISTORY_DESCRIPTORS * HISTORY_DESCRIPTOR_BYTES
                + MANIFEST_DIGEST_BYTES
        );
    }

    #[test]
    fn object_tags_are_shared_without_broadening_import_kinds() -> Result<(), Error> {
        for (tag, kind) in [
            (1, ObjectType::Mailbox),
            (2, ObjectType::Thread),
            (3, ObjectType::Email),
            (4, ObjectType::Identity),
            (5, ObjectType::EmailSubmission),
        ] {
            assert_eq!(kind.tag(), tag);
            assert_eq!(ObjectType::from_tag(u16::from(tag))?, kind);
        }
        assert_eq!(key::SourceKind::Email.tag(), ObjectType::Email.tag());
        assert_eq!(key::SourceKind::Mailbox.tag(), ObjectType::Mailbox.tag());
        for tag in [0, 2, 4, 5, 6, u8::MAX] {
            assert_eq!(key::SourceKind::from_tag(tag), Err(Error::InvalidTag));
        }
        assert_eq!(ObjectType::from_tag(0), Err(Error::InvalidTag));
        assert_eq!(ObjectType::from_tag(6), Err(Error::InvalidTag));
        Ok(())
    }
}
