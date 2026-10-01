//! Bounded contiguous supplied journal frames; the caller owns I/O and EOF.
use super::{
    container::{Error as ContainerError, JournalHeader},
    frame::{CheckedHeader, DecodeError, Frame},
    Error as FormatError, Sequence, JOURNAL_HEADER_BYTES, MAX_JOURNAL_FRAME_BYTES,
    MAX_JOURNAL_OPERATIONS, MIN_FRAME_BYTES,
};
use crate::ports::{Crypto, CryptoError, Digest};

/// Retains the distinction between frame errors and journal-wide validation errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Frame(DecodeError),
    Journal(ContainerError),
}
impl From<DecodeError> for Error {
    fn from(error: DecodeError) -> Self {
        Self::Frame(error)
    }
}
impl From<ContainerError> for Error {
    fn from(error: ContainerError) -> Self {
        Self::Journal(error)
    }
}
impl From<FormatError> for Error {
    fn from(error: FormatError) -> Self {
        Self::Journal(error.into())
    }
}
impl From<CryptoError> for Error {
    fn from(error: CryptoError) -> Self {
        Self::Journal(error.into())
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Frame(error) => error.fmt(f),
            Self::Journal(error) => write!(f, "journal validation failed: {error}"),
        }
    }
}
impl std::error::Error for Error {}

/// Complete supplied stream, not proof of physical EOF or manifest selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Summary {
    header: JournalHeader,
    through: Sequence,
    frame_bytes: usize,
    operations: usize,
    digest: [u8; 32],
}
impl Summary {
    pub const fn header(self) -> JournalHeader {
        self.header
    }
    pub const fn through(self) -> Sequence {
        self.through
    }
    pub const fn frame_bytes(self) -> usize {
        self.frame_bytes
    }
    pub const fn operations(self) -> usize {
        self.operations
    }
    pub fn file_bytes(self) -> Result<usize, FormatError> {
        self.frame_bytes
            .checked_add(JOURNAL_HEADER_BYTES)
            .ok_or(FormatError::Overflow)
    }
    /// SHA-256 includes the journal header and all complete supplied frames.
    pub const fn digest(self) -> [u8; 32] {
        self.digest
    }
}

/// Each push is one exact frame; any returned error permanently fails the stream.
/// A frame remains provisional until final-view checks and selected-file checks pass.
pub struct Verifier<'c, C: Crypto> {
    crypto: &'c C,
    header: JournalHeader,
    through: Sequence,
    frame_bytes: usize,
    operations: usize,
    digest: C::Sha256,
    failed: Option<Error>,
}
impl<'c, C: Crypto> Verifier<'c, C> {
    pub fn new(crypto: &'c C, bytes: &[u8]) -> Result<Self, Error> {
        let header = JournalHeader::decode(crypto, bytes)?;
        let mut digest = crypto.sha256()?;
        digest.update(bytes)?;
        Ok(Self {
            crypto,
            header,
            through: header.base,
            frame_bytes: 0,
            operations: 0,
            digest,
            failed: None,
        })
    }
    pub fn push<'a>(&mut self, bytes: &'a [u8]) -> Result<Frame<'a>, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.push_frame(bytes);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn push_frame<'a>(&mut self, bytes: &'a [u8]) -> Result<Frame<'a>, Error> {
        self.through.successor()?;
        let remaining = MAX_JOURNAL_FRAME_BYTES
            .checked_sub(self.frame_bytes)
            .ok_or(FormatError::Overflow)?;
        if remaining < MIN_FRAME_BYTES || self.operations == MAX_JOURNAL_OPERATIONS {
            return Err(FormatError::Limit.into());
        }
        let checked = CheckedHeader::read(self.crypto, self.through, bytes)?;
        let header = checked.header();
        let frame_bytes = self
            .frame_bytes
            .checked_add(header.frame_bytes)
            .ok_or(FormatError::Overflow)?;
        if frame_bytes > MAX_JOURNAL_FRAME_BYTES {
            return Err(FormatError::Limit.into());
        }
        let operations = self
            .operations
            .checked_add(header.operations)
            .ok_or(FormatError::Overflow)?;
        if operations > MAX_JOURNAL_OPERATIONS {
            return Err(FormatError::Limit.into());
        }
        let frame = checked.finish(self.crypto)?;
        self.digest.update(bytes)?;
        self.through = header.sequence;
        self.frame_bytes = frame_bytes;
        self.operations = operations;
        Ok(frame)
    }
    /// Consumes the stream. The caller must separately establish expected extent and EOF.
    pub fn finish(self) -> Result<Summary, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        Ok(Summary {
            header: self.header,
            through: self.through,
            frame_bytes: self.frame_bytes,
            operations: self.operations,
            digest: self.digest.finish()?,
        })
    }
}
