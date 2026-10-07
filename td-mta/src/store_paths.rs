//! Canonical relative storage names. Names grant no filesystem or account authority.
use crate::{
    bounded::TextBuffer,
    format::row::BlobKind,
    ids::{AccountId, BlobId},
};
use std::{ffi::CStr, fmt, path::Path};

/// Longest blob path is 90 bytes; reserve 128 plus one NUL byte.
pub const CAPACITY: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidNumber,
    InvalidBlobName,
    Encoding,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidNumber => "noncanonical temporary object number",
            Self::InvalidBlobName => "noncanonical blob filename or wrong shard",
            Self::Encoding => "generated storage path exceeds its encoding bounds",
        })
    }
}
impl std::error::Error for Error {}

/// Positive temporary object number, rendered with exactly twenty decimal digits.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Number(u64);
impl Number {
    pub const fn new(value: u64) -> Result<Self, Error> {
        if value == 0 {
            Err(Error::InvalidNumber)
        } else {
            Ok(Self(value))
        }
    }
    pub const fn value(self) -> u64 {
        self.0
    }
    /// Parse the whole numeric component, without a journal suffix.
    pub fn parse(text: &str) -> Result<Self, Error> {
        if text.len() != 20 {
            return Err(Error::InvalidNumber);
        }
        let mut value = 0_u64;
        for byte in text.bytes() {
            if !byte.is_ascii_digit() {
                return Err(Error::InvalidNumber);
            }
            value = value
                .checked_mul(10)
                .and_then(|n| n.checked_add(u64::from(byte - b'0')))
                .ok_or(Error::InvalidNumber)?;
        }
        Self::new(value)
    }
}
impl fmt::Display for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:020}", self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootEntry {
    Database,
    Wal,
    SharedMemory,
    Lock,
    Accounts,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountEntry {
    Root,
    Messages,
    Uploads,
    Temporary,
    TemporaryFile(Number),
    Shard(BlobKind, u8),
    Blob(BlobKind, BlobId),
}

/// Fixed owned bytes, constructed only from typed identifiers and known literals.
/// Accessors borrow the generated name; there is no arbitrary append/format API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Name {
    bytes: [u8; CAPACITY + 1],
    length: usize,
}
impl Name {
    fn encode(arguments: fmt::Arguments<'_>) -> Result<Self, Error> {
        let mut name = Self {
            bytes: [0; CAPACITY + 1],
            length: 0,
        };
        let mut output = TextBuffer::new(name.bytes.get_mut(..CAPACITY).ok_or(Error::Encoding)?);
        output.format(arguments).map_err(|_| Error::Encoding)?;
        name.length = output.len();
        Ok(name)
    }
    pub fn root(entry: RootEntry) -> Result<Self, Error> {
        let text = match entry {
            RootEntry::Database => "metadata.sqlite3",
            RootEntry::Wal => "metadata.sqlite3-wal",
            RootEntry::SharedMemory => "metadata.sqlite3-shm",
            RootEntry::Lock => "LOCK",
            RootEntry::Accounts => "accounts",
        };
        Self::encode(format_args!("{text}"))
    }
    pub fn account(account: AccountId, entry: AccountEntry) -> Result<Self, Error> {
        let suffix = match entry {
            AccountEntry::Root => return Self::encode(format_args!("accounts/{account}")),
            AccountEntry::TemporaryFile(number) => {
                return Self::encode(format_args!("accounts/{account}/tmp/{number}.tmp"))
            }
            AccountEntry::Shard(kind, shard) => {
                return Self::encode(format_args!(
                    "accounts/{account}/{}/{shard:02x}",
                    blob_directory(kind)
                ))
            }
            AccountEntry::Blob(kind, blob) => {
                let shard = blob.as_bytes().first().ok_or(Error::Encoding)?;
                return Self::encode(format_args!(
                    "accounts/{account}/{}/{shard:02x}/{blob}{}",
                    blob_directory(kind),
                    blob_suffix(kind)
                ));
            }
            AccountEntry::Messages => "messages",
            AccountEntry::Uploads => "uploads",
            AccountEntry::Temporary => "tmp",
        };
        Self::encode(format_args!("accounts/{account}/{suffix}"))
    }
    pub fn as_bytes(&self) -> Result<&[u8], Error> {
        self.bytes.get(..self.length).ok_or(Error::Encoding)
    }
    pub fn as_str(&self) -> Result<&str, Error> {
        std::str::from_utf8(self.as_bytes()?).map_err(|_| Error::Encoding)
    }
    pub fn as_path(&self) -> Result<&Path, Error> {
        Ok(Path::new(self.as_str()?))
    }
    pub fn as_c_str(&self) -> Result<&CStr, Error> {
        let end = self.length.checked_add(1).ok_or(Error::Encoding)?;
        CStr::from_bytes_with_nul(self.bytes.get(..end).ok_or(Error::Encoding)?)
            .map_err(|_| Error::Encoding)
    }
}

fn blob_directory(kind: BlobKind) -> &'static str {
    match kind {
        BlobKind::Message => "messages",
        BlobKind::Upload => "uploads",
    }
}
fn blob_suffix(kind: BlobKind) -> &'static str {
    match kind {
        BlobKind::Message => ".eml",
        BlobKind::Upload => ".blob",
    }
}

/// Validate a directory-entry basename against its known namespace and shard.
/// A valid filename is not proof that the blob is live, immutable or authorized.
pub fn parse_blob_name(kind: BlobKind, shard: u8, text: &str) -> Result<BlobId, Error> {
    let id = text
        .strip_suffix(blob_suffix(kind))
        .ok_or(Error::InvalidBlobName)?;
    let blob = BlobId::parse(id).map_err(|_| Error::InvalidBlobName)?;
    if blob.as_bytes().first() != Some(&shard) {
        return Err(Error::InvalidBlobName);
    }
    Ok(blob)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn longest_retained_blob_name_fits_its_exact_capacity() -> Result<(), Error> {
        let name = Name::account(
            AccountId::from_bytes([255; 16]),
            AccountEntry::Blob(BlobKind::Message, BlobId::from_bytes([255; 16])),
        )?;
        assert_eq!(name.as_path()?.as_os_str().len(), 90);
        assert!(name.as_path()?.as_os_str().len() < CAPACITY);
        assert_eq!(
            name.as_c_str()?.to_bytes(),
            name.as_path()?.as_os_str().as_encoded_bytes()
        );
        Ok(())
    }
}
