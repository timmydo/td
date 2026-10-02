//! Incremental supplied frame bytes; entries remain provisional until completion.
use super::{
    container::Error,
    frame::{decode_operation, Entry, END},
    frame_header::Header,
    Error as FormatError, Sequence, FRAME_FOOTER_BYTES,
};
use crate::ports::{Crypto, Digest};

/// Complete local frame grammar and integrity, without I/O or final-view proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Summary {
    header: Header,
    digest: [u8; 32],
}
impl Summary {
    pub const fn header(self) -> Header {
        self.header
    }
    /// The footer digest: header, operations and end magic, excluding itself.
    pub const fn digest(self) -> [u8; 32] {
        self.digest
    }
}

/// Supply an exact header, one exact operation per push, then an exact footer.
/// Errors permanently fail the stream. No operation bytes are retained.
pub struct Verifier<'c, C: Crypto> {
    crypto: &'c C,
    header: Header,
    digest: C::Sha256,
    operations: usize,
    payload: usize,
    failed: Option<Error>,
}
impl<'c, C: Crypto> Verifier<'c, C> {
    pub fn new(crypto: &'c C, previous: Sequence, bytes: &[u8]) -> Result<Self, Error> {
        let expected = previous.successor()?;
        let header = Header::decode(crypto, bytes)?;
        if header.sequence != expected {
            return Err(FormatError::InvalidValue.into());
        }
        let mut digest = crypto.sha256()?;
        digest.update(bytes)?;
        Ok(Self {
            crypto,
            header,
            digest,
            operations: 0,
            payload: 0,
            failed: None,
        })
    }
    /// Validated header only; operations and complete-frame integrity remain unchecked.
    pub const fn header(&self) -> Header {
        self.header
    }
    /// Returned entries borrow only this input and remain provisional until finish.
    pub fn push<'a>(&mut self, bytes: &'a [u8]) -> Result<Entry<'a>, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.push_operation(bytes);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn push_operation<'a>(&mut self, bytes: &'a [u8]) -> Result<Entry<'a>, Error> {
        let operations = self
            .operations
            .checked_add(1)
            .ok_or(FormatError::Overflow)?;
        let payload = self
            .payload
            .checked_add(bytes.len())
            .ok_or(FormatError::Overflow)?;
        if operations > self.header.operations || payload > self.header.payload_bytes()? {
            return Err(FormatError::TrailingBytes.into());
        }
        let operation = decode_operation(bytes)?;
        self.digest.update(bytes)?;
        let ordinal = self.operations;
        self.operations = operations;
        self.payload = payload;
        Ok(Entry { ordinal, operation })
    }
    /// Consumes this verifier; no physical EOF, selection or repair authority is granted.
    pub fn finish(mut self, footer: &[u8]) -> Result<Summary, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.operations != self.header.operations
            || self.payload != self.header.payload_bytes()?
        {
            return Err(FormatError::Truncated.into());
        }
        match footer.len().cmp(&FRAME_FOOTER_BYTES) {
            std::cmp::Ordering::Less => return Err(FormatError::Truncated.into()),
            std::cmp::Ordering::Greater => return Err(FormatError::TrailingBytes.into()),
            std::cmp::Ordering::Equal => {}
        }
        let (magic, checksum) = footer
            .split_at_checked(END.len())
            .ok_or(FormatError::Truncated)?;
        if magic != END {
            return Err(FormatError::InvalidTag.into());
        }
        let checksum = checksum.try_into().map_err(|_| FormatError::Truncated)?;
        self.digest.update(magic)?;
        let digest = self.digest.finish()?;
        if !self.crypto.equal_digest(&digest, &checksum) {
            return Err(Error::Checksum);
        }
        Ok(Summary {
            header: self.header,
            digest,
        })
    }
}
