//! Bounded manifest structure. Referenced files and CURRENT remain unchecked.
use super::{
    container::{capacity, checked_preimage, publish, Error},
    scalar::{Reader, Writer},
    table::TableHeader,
    Error as FormatError, Sequence, Table, HISTORY_DESCRIPTOR_BYTES, MANIFEST_PREFIX_BYTES,
    MAX_HISTORY_DESCRIPTORS, MAX_MANIFEST_BYTES, TABLE_COUNT, TABLE_DESCRIPTOR_BYTES,
};
use crate::{
    ids::{AccountId, StoreEpoch},
    ports::Crypto,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    pub account: AccountId,
    pub epoch: StoreEpoch,
    pub generation: u64,
    pub through: Sequence,
    pub active_segment: u64,
}
impl Header {
    fn validate(self) -> Result<Self, FormatError> {
        if self.generation == 0 || self.active_segment == 0 {
            return Err(FormatError::InvalidValue);
        }
        Ok(self)
    }
    fn table_header(self, descriptor: TableDescriptor) -> Result<TableHeader, FormatError> {
        TableHeader {
            table: descriptor.table,
            account: self.account,
            epoch: self.epoch,
            generation: self.generation,
            through: self.through,
            record_count: descriptor.record_count,
            payload_bytes: descriptor
                .file_bytes
                .checked_sub(super::TABLE_HEADER_BYTES as u64)
                .ok_or(FormatError::InvalidValue)?,
        }
        .validate()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TableDescriptor {
    pub table: Table,
    pub record_count: u64,
    pub file_bytes: u64,
    pub digest: [u8; 32],
}
impl TableDescriptor {
    fn read(reader: &mut Reader<'_>) -> Result<Self, FormatError> {
        let table = Table::from_tag(reader.u16()?)?;
        if reader.u16()? != super::SCHEMA_VERSION || reader.u32()? != 0 {
            return Err(FormatError::InvalidTag);
        }
        Ok(Self {
            table,
            record_count: reader.u64()?,
            file_bytes: reader.u64()?,
            digest: reader.fixed()?,
        })
    }
    fn write(self, writer: &mut Writer<'_>) -> Result<(), FormatError> {
        writer.u16(self.table.tag())?;
        writer.u16(super::SCHEMA_VERSION)?;
        writer.u32(0)?;
        writer.u64(self.record_count)?;
        writer.u64(self.file_bytes)?;
        writer.put(&self.digest)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryDescriptor {
    pub segment: u64,
    pub base: Sequence,
    pub through: Sequence,
    pub file_bytes: u64,
    pub digest: [u8; 32],
}
impl HistoryDescriptor {
    fn validate(self, header: Header) -> Result<(), FormatError> {
        if self.segment == 0
            || self.segment == header.active_segment
            || self.through <= self.base
            || self.through > header.through
        {
            return Err(FormatError::InvalidValue);
        }
        let frames = self
            .through
            .number()
            .checked_sub(self.base.number())
            .ok_or(FormatError::InvalidValue)?;
        let payload = self
            .file_bytes
            .checked_sub(super::JOURNAL_HEADER_BYTES as u64)
            .ok_or(FormatError::InvalidValue)?;
        let minimum = frames
            .checked_mul(super::MIN_FRAME_BYTES as u64)
            .ok_or(FormatError::Overflow)?;
        let maximum = frames.checked_mul(super::MAX_FRAME_BYTES as u64);
        if payload < minimum || maximum.is_some_and(|n| payload > n) {
            return Err(FormatError::InvalidValue);
        }
        Ok(())
    }
    fn read(reader: &mut Reader<'_>) -> Result<Self, FormatError> {
        Ok(Self {
            segment: reader.u64()?,
            base: Sequence::from_u64(reader.u64()?),
            through: Sequence::from_u64(reader.u64()?),
            file_bytes: reader.u64()?,
            digest: reader.fixed()?,
        })
    }
    fn write(self, writer: &mut Writer<'_>) -> Result<(), FormatError> {
        writer.u64(self.segment)?;
        writer.u64(self.base.number())?;
        writer.u64(self.through.number())?;
        writer.u64(self.file_bytes)?;
        writer.put(&self.digest)
    }
}

const HISTORY_START: usize = MANIFEST_PREFIX_BYTES + TABLE_COUNT * TABLE_DESCRIPTOR_BYTES;
fn extent(count: usize) -> Result<usize, FormatError> {
    if count > MAX_HISTORY_DESCRIPTORS {
        return Err(FormatError::Limit);
    }
    count
        .checked_mul(HISTORY_DESCRIPTOR_BYTES)
        .and_then(|n| n.checked_add(HISTORY_START + super::MANIFEST_DIGEST_BYTES))
        .ok_or(FormatError::Overflow)
}
fn history_at(bytes: &[u8], index: usize) -> Result<HistoryDescriptor, FormatError> {
    let start = index
        .checked_mul(HISTORY_DESCRIPTOR_BYTES)
        .and_then(|n| n.checked_add(HISTORY_START))
        .ok_or(FormatError::Overflow)?;
    let end = start
        .checked_add(HISTORY_DESCRIPTOR_BYTES)
        .ok_or(FormatError::Overflow)?;
    HistoryDescriptor::read(&mut Reader::new(
        bytes.get(start..end).ok_or(FormatError::Truncated)?,
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Manifest<'a> {
    bytes: &'a [u8],
    header: Header,
    history_count: usize,
}
impl<'a> Manifest<'a> {
    pub fn decode(crypto: &impl Crypto, bytes: &'a [u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(FormatError::Limit.into());
        }
        // Untrusted count bounds only the extent; no descriptor is exposed before hashing.
        let count_bytes = bytes.get(84..88).ok_or(FormatError::Truncated)?;
        let count =
            usize::try_from(Reader::new(count_bytes).u32()?).map_err(|_| FormatError::Overflow)?;
        let mut reader = Reader::new(checked_preimage(crypto, bytes, extent(count)?)?);
        if reader.fixed::<8>()? != *b"TDMTMAN1"
            || reader.u16()? != super::CONTAINER_VERSION
            || reader.u16()? != super::SCHEMA_VERSION
            || reader.u32()? != 0
        {
            return Err(FormatError::InvalidTag.into());
        }
        let header = Header {
            account: AccountId::from_bytes(reader.fixed()?),
            epoch: StoreEpoch::from_bytes(reader.fixed()?),
            generation: reader.u64()?,
            through: Sequence::from_u64(reader.u64()?),
            active_segment: reader.u64()?,
        }
        .validate()?;
        if reader.u64()? != header.through.number()
            || reader.u32()? != TABLE_COUNT as u32
            || reader.u32()? != u32::try_from(count).map_err(|_| FormatError::Overflow)?
        {
            return Err(FormatError::InvalidValue.into());
        }
        for tag in 1..=TABLE_COUNT {
            let descriptor = TableDescriptor::read(&mut reader)?;
            if usize::from(descriptor.table.tag()) != tag {
                return Err(FormatError::InvalidValue.into());
            }
            header.table_header(descriptor)?;
        }
        let mut previous = None;
        for index in 0..count {
            let descriptor = HistoryDescriptor::read(&mut reader)?;
            descriptor.validate(header)?;
            if previous.is_some_and(|end| end != descriptor.base) {
                return Err(FormatError::InvalidValue.into());
            }
            for earlier in 0..index {
                if history_at(bytes, earlier)?.segment == descriptor.segment {
                    return Err(FormatError::InvalidValue.into());
                }
            }
            previous = Some(descriptor.through);
        }
        if previous.is_some_and(|end| end != header.through) {
            return Err(FormatError::InvalidValue.into());
        }
        reader.finish()?;
        Ok(Self {
            bytes,
            header,
            history_count: count,
        })
    }
    pub const fn header(self) -> Header {
        self.header
    }
    pub const fn history_count(self) -> usize {
        self.history_count
    }
    pub fn table(self, table: Table) -> Result<TableDescriptor, FormatError> {
        let index = usize::from(table.tag())
            .checked_sub(1)
            .ok_or(FormatError::InvalidTag)?;
        if index >= TABLE_COUNT {
            return Err(FormatError::InvalidTag);
        }
        let start = index
            .checked_mul(TABLE_DESCRIPTOR_BYTES)
            .and_then(|n| n.checked_add(MANIFEST_PREFIX_BYTES))
            .ok_or(FormatError::Overflow)?;
        let end = start
            .checked_add(TABLE_DESCRIPTOR_BYTES)
            .ok_or(FormatError::Overflow)?;
        TableDescriptor::read(&mut Reader::new(
            self.bytes.get(start..end).ok_or(FormatError::Truncated)?,
        ))
    }
    pub fn history(self, index: usize) -> Result<HistoryDescriptor, FormatError> {
        if index >= self.history_count {
            return Err(FormatError::InvalidValue);
        }
        history_at(self.bytes, index)
    }
}

/// Fixed scratch, no heap; returned errors preserve output and success preserves its suffix.
pub fn encode(
    crypto: &impl Crypto,
    header: Header,
    tables: &[TableDescriptor; TABLE_COUNT],
    history: &[HistoryDescriptor],
    output: &mut [u8],
) -> Result<usize, Error> {
    header.validate()?;
    let length = extent(history.len())?;
    capacity(output, length)?;
    let mut scratch = [0; MAX_MANIFEST_BYTES];
    let scratch = scratch.get_mut(..length).ok_or(FormatError::OutputFull)?;
    let mut writer = Writer::new(scratch);
    writer.put(b"TDMTMAN1")?;
    writer.u16(super::CONTAINER_VERSION)?;
    writer.u16(super::SCHEMA_VERSION)?;
    writer.u32(0)?;
    writer.put(header.account.as_bytes())?;
    writer.put(header.epoch.as_bytes())?;
    writer.u64(header.generation)?;
    writer.u64(header.through.number())?;
    writer.u64(header.active_segment)?;
    writer.u64(header.through.number())?;
    writer.u32(TABLE_COUNT as u32)?;
    writer.u32(u32::try_from(history.len()).map_err(|_| FormatError::Overflow)?)?;
    for (index, descriptor) in tables.iter().enumerate() {
        if usize::from(descriptor.table.tag()) != index + 1 {
            return Err(FormatError::InvalidValue.into());
        }
        header.table_header(*descriptor)?;
        descriptor.write(&mut writer)?;
    }
    let mut previous = None;
    for (index, descriptor) in history.iter().enumerate() {
        descriptor.validate(header)?;
        if previous.is_some_and(|end| end != descriptor.base) {
            return Err(FormatError::InvalidValue.into());
        }
        for earlier in history.get(..index).ok_or(FormatError::InvalidValue)? {
            if earlier.segment == descriptor.segment {
                return Err(FormatError::InvalidValue.into());
            }
        }
        descriptor.write(&mut writer)?;
        previous = Some(descriptor.through);
    }
    if previous.is_some_and(|end| end != header.through) {
        return Err(FormatError::InvalidValue.into());
    }
    publish(crypto, scratch, output)
}
