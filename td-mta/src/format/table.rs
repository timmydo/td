//! Checked table headers and individual records, not a complete table verifier.
use super::{
    container::{capacity, checked_preimage, publish, Error},
    row,
    scalar::{Reader, Writer},
    Error as FormatError, Sequence, Table,
};
use crate::{
    ids::{AccountId, StoreEpoch},
    ports::{Crypto, Digest, Record as RowRecord},
};

pub const RECORD_PREFIX_BYTES: usize = 16;
pub const MIN_RECORD_BYTES: usize = super::RECORD_OVERHEAD_BYTES + super::MIN_KEY_BYTES;
pub const MAX_RECORD_BYTES: usize =
    super::RECORD_OVERHEAD_BYTES + super::MAX_KEY_BYTES + super::MAX_VALUE_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TableHeader {
    pub table: Table,
    pub account: AccountId,
    pub epoch: StoreEpoch,
    pub generation: u64,
    pub through: Sequence,
    pub record_count: u64,
    pub payload_bytes: u64,
}
impl TableHeader {
    fn validate(self) -> Result<Self, FormatError> {
        if self.generation == 0
            || (self.record_count == 0) != (self.payload_bytes == 0)
            || (self.through.number() == 0 && self.record_count != 0)
        {
            return Err(FormatError::InvalidValue);
        }
        let minimum = self
            .record_count
            .checked_mul(MIN_RECORD_BYTES as u64)
            .ok_or(FormatError::Overflow)?;
        // An overflowing mathematical upper bound cannot exclude a u64 payload.
        let maximum = self.record_count.checked_mul(MAX_RECORD_BYTES as u64);
        if self.payload_bytes < minimum || maximum.is_some_and(|n| self.payload_bytes > n) {
            return Err(FormatError::InvalidValue);
        }
        self.file_bytes()?;
        Ok(self)
    }
    /// Arithmetic only; does not prove an actual file has this extent or content.
    pub fn file_bytes(self) -> Result<u64, FormatError> {
        self.payload_bytes
            .checked_add(super::TABLE_HEADER_BYTES as u64)
            .ok_or(FormatError::Overflow)
    }
    pub fn decode(crypto: &impl Crypto, bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(checked_preimage(crypto, bytes, super::TABLE_HEADER_BYTES)?);
        if reader.fixed::<8>()? != *b"TDMTTBL1"
            || reader.u16()? != super::CONTAINER_VERSION
            || reader.u16()? != super::SCHEMA_VERSION
        {
            return Err(FormatError::InvalidTag.into());
        }
        let table = Table::from_tag(reader.u16()?)?;
        if reader.u16()? != 0 {
            return Err(FormatError::InvalidTag.into());
        }
        let value = Self {
            table,
            account: AccountId::from_bytes(reader.fixed()?),
            epoch: StoreEpoch::from_bytes(reader.fixed()?),
            generation: reader.u64()?,
            through: Sequence::from_u64(reader.u64()?),
            record_count: reader.u64()?,
            payload_bytes: reader.u64()?,
        }
        .validate()?;
        reader.finish()?;
        Ok(value)
    }
    /// Returned errors preserve output; bytes after the header remain untouched.
    pub fn encode(self, crypto: &impl Crypto, output: &mut [u8]) -> Result<usize, Error> {
        self.validate()?;
        capacity(output, super::TABLE_HEADER_BYTES)?;
        let mut scratch = [0; super::TABLE_HEADER_BYTES];
        let mut writer = Writer::new(&mut scratch);
        writer.put(b"TDMTTBL1")?;
        writer.u16(super::CONTAINER_VERSION)?;
        writer.u16(super::SCHEMA_VERSION)?;
        writer.u16(self.table.tag())?;
        writer.u16(0)?;
        writer.put(self.account.as_bytes())?;
        writer.put(self.epoch.as_bytes())?;
        writer.u64(self.generation)?;
        writer.u64(self.through.number())?;
        writer.u64(self.record_count)?;
        writer.u64(self.payload_bytes)?;
        publish(crypto, &mut scratch, output)
    }
}

fn record_bytes(key: usize, value: usize) -> Result<usize, FormatError> {
    if !(super::MIN_KEY_BYTES..=super::MAX_KEY_BYTES).contains(&key)
        || value > super::MAX_VALUE_BYTES
    {
        return Err(FormatError::Limit);
    }
    super::RECORD_OVERHEAD_BYTES
        .checked_add(key)
        .and_then(|n| n.checked_add(value))
        .ok_or(FormatError::Overflow)
}

/// Bound one record from its exact 16-byte prefix; this grants no integrity.
pub fn record_extent(prefix: &[u8]) -> Result<usize, FormatError> {
    let mut reader = Reader::new(prefix);
    let key = usize::try_from(reader.u32()?).map_err(|_| FormatError::Overflow)?;
    let value = usize::try_from(reader.u32()?).map_err(|_| FormatError::Overflow)?;
    reader.u64()?;
    reader.finish()?;
    record_bytes(key, value)
}

/// Locally valid row bytes; references, table ordering and file bindings remain unchecked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Record<'a> {
    row: RowRecord<'a>,
    key: &'a [u8],
    value: &'a [u8],
}
impl<'a> Record<'a> {
    pub fn new(
        table: Table,
        changed: Sequence,
        key: &'a [u8],
        value: &'a [u8],
    ) -> Result<Self, Error> {
        record_bytes(key.len(), value.len())?;
        if changed.number() == 0 {
            return Err(FormatError::InvalidValue.into());
        }
        let (typed_key, row) = row::decode_record(table, key, value)?;
        Ok(Self {
            row: RowRecord {
                key: typed_key,
                row,
                last_change: changed,
            },
            key,
            value,
        })
    }
    pub const fn row(self) -> RowRecord<'a> {
        self.row
    }
    /// Original bytes supply unsigned bytewise table ordering, not displayed IDs.
    pub const fn key_bytes(self) -> &'a [u8] {
        self.key
    }
    pub const fn value_bytes(self) -> &'a [u8] {
        self.value
    }
    pub fn encoded_len(self) -> Result<usize, FormatError> {
        record_bytes(self.key.len(), self.value.len())
    }
    fn checkpoint(self, through: Sequence) -> Result<Self, FormatError> {
        if self.row.last_change > through {
            return Err(FormatError::InvalidValue);
        }
        Ok(self)
    }
    /// Prefix lengths only bound the read; checksum and full row grammar still must pass.
    pub fn decode(
        crypto: &impl Crypto,
        table: Table,
        through: Sequence,
        bytes: &'a [u8],
    ) -> Result<Self, Error> {
        let prefix = bytes
            .get(..RECORD_PREFIX_BYTES)
            .ok_or(FormatError::Truncated)?;
        let length = record_extent(prefix)?;
        let preimage = checked_preimage(crypto, bytes, length)?;
        let mut reader = Reader::new(preimage);
        let key_len = usize::try_from(reader.u32()?).map_err(|_| FormatError::Overflow)?;
        let value_len = usize::try_from(reader.u32()?).map_err(|_| FormatError::Overflow)?;
        let changed = Sequence::from_u64(reader.u64()?);
        let key = reader.take(key_len)?;
        let value = reader.take(value_len)?;
        reader.finish()?;
        Ok(Self::new(table, changed, key, value)?.checkpoint(through)?)
    }
    /// Hash before writing; no record-sized scratch allocation is needed.
    /// Returned errors preserve output, including provider errors.
    pub fn encode(
        self,
        crypto: &impl Crypto,
        through: Sequence,
        output: &mut [u8],
    ) -> Result<usize, Error> {
        self.checkpoint(through)?;
        let length = self.encoded_len()?;
        capacity(output, length)?;
        let mut prefix = [0; RECORD_PREFIX_BYTES];
        let mut writer = Writer::new(&mut prefix);
        writer.u32(u32::try_from(self.key.len()).map_err(|_| FormatError::Overflow)?)?;
        writer.u32(u32::try_from(self.value.len()).map_err(|_| FormatError::Overflow)?)?;
        writer.u64(self.row.last_change.number())?;
        let mut digest = crypto.sha256()?;
        digest.update(&prefix)?;
        digest.update(self.key)?;
        digest.update(self.value)?;
        let checksum = digest.finish()?;
        let mut writer = Writer::new(output);
        writer.put(&prefix)?;
        writer.put(self.key)?;
        writer.put(self.value)?;
        writer.put(&checksum)?;
        Ok(writer.written())
    }
}
