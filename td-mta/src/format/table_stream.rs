//! Fixed-state validation of supplied table bytes; the caller owns I/O and EOF.
use super::{
    container::Error,
    table::{Record, TableHeader},
    Error as FormatError, MAX_KEY_BYTES,
};
use crate::ports::{Crypto, Digest};

/// Complete supplied stream; selected-manifest bindings remain unverified.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Summary {
    header: TableHeader,
    digest: [u8; 32],
}
impl Summary {
    pub const fn header(self) -> TableHeader {
        self.header
    }
    /// SHA-256 includes the table header and every complete record, including its checksum.
    pub const fn digest(self) -> [u8; 32] {
        self.digest
    }
}

/// Feed exact records in file order; a returned error permanently fails the stream.
/// Record results are provisional until finish and external file bindings pass.
pub struct Verifier<'c, C: Crypto> {
    crypto: &'c C,
    header: TableHeader,
    digest: C::Sha256,
    previous: [u8; MAX_KEY_BYTES],
    previous_len: usize,
    records: u64,
    payload: u64,
    failed: Option<Error>,
}
impl<'c, C: Crypto> Verifier<'c, C> {
    pub fn new(crypto: &'c C, header_bytes: &[u8]) -> Result<Self, Error> {
        let header = TableHeader::decode(crypto, header_bytes)?;
        let mut digest = crypto.sha256()?;
        digest.update(header_bytes)?;
        Ok(Self {
            crypto,
            header,
            digest,
            previous: [0; MAX_KEY_BYTES],
            previous_len: 0,
            records: 0,
            payload: 0,
            failed: None,
        })
    }
    /// Validated header only; records, physical extent and selected binding remain unchecked.
    pub const fn header(&self) -> TableHeader {
        self.header
    }
    pub fn push<'a>(&mut self, bytes: &'a [u8]) -> Result<Record<'a>, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.push_record(bytes);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn push_record<'a>(&mut self, bytes: &'a [u8]) -> Result<Record<'a>, Error> {
        let records = self.records.checked_add(1).ok_or(FormatError::Overflow)?;
        let length = u64::try_from(bytes.len()).map_err(|_| FormatError::Overflow)?;
        let payload = self
            .payload
            .checked_add(length)
            .ok_or(FormatError::Overflow)?;
        if records > self.header.record_count || payload > self.header.payload_bytes {
            return Err(FormatError::TrailingBytes.into());
        }
        let record = Record::decode(self.crypto, self.header.table, self.header.through, bytes)?;
        let key = record.key_bytes();
        let previous = self
            .previous
            .get(..self.previous_len)
            .ok_or(FormatError::InvalidValue)?;
        if self.records != 0 && previous >= key {
            return Err(FormatError::InvalidValue.into());
        }
        let destination = self
            .previous
            .get_mut(..key.len())
            .ok_or(FormatError::Limit)?;
        self.digest.update(bytes)?;
        destination.copy_from_slice(key);
        self.previous_len = key.len();
        self.records = records;
        self.payload = payload;
        Ok(record)
    }
    /// Consumes the stream; does not read or establish physical EOF.
    pub fn finish(self) -> Result<Summary, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.records != self.header.record_count || self.payload != self.header.payload_bytes {
            return Err(FormatError::Truncated.into());
        }
        Ok(Summary {
            header: self.header,
            digest: self.digest.finish()?,
        })
    }
}
