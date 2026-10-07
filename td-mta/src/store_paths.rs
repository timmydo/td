//! Canonical relative storage names. Names grant no filesystem or account authority.
use crate::bounded::TextBuffer;
use std::{ffi::CStr, fmt, path::Path};

/// Fixed path capacity includes headroom for known database sidecar names.
pub const CAPACITY: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Encoding,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Encoding => "generated storage path exceeds its encoding bounds",
        })
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootEntry {
    Database,
    Wal,
    SharedMemory,
    Lock,
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
        };
        Self::encode(format_args!("{text}"))
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
