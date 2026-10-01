//! Exact fixed-container codecs. Integrity is not cross-file consistency or durability.
use super::{
    scalar::{Reader, Writer},
    Error as FormatError, Sequence,
};
use crate::{
    ids::{AccountId, InstanceId, StoreEpoch},
    ports::{Crypto, CryptoError, Digest},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Format(FormatError),
    Crypto(CryptoError),
    Checksum,
}
impl From<FormatError> for Error {
    fn from(error: FormatError) -> Self {
        Self::Format(error)
    }
}
impl From<CryptoError> for Error {
    fn from(error: CryptoError) -> Self {
        Self::Crypto(error)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Format(error) => error.fmt(f),
            Self::Crypto(error) => error.fmt(f),
            Self::Checksum => f.write_str("container checksum mismatch"),
        }
    }
}
impl std::error::Error for Error {}

fn hash(crypto: &impl Crypto, bytes: &[u8]) -> Result<[u8; 32], Error> {
    let mut digest = crypto.sha256()?;
    digest.update(bytes)?;
    Ok(digest.finish()?)
}

fn read<'a>(
    crypto: &impl Crypto,
    bytes: &'a [u8],
    length: usize,
    magic: &[u8; 8],
) -> Result<Reader<'a>, Error> {
    match bytes.len().cmp(&length) {
        std::cmp::Ordering::Less => return Err(FormatError::Truncated.into()),
        std::cmp::Ordering::Greater => return Err(FormatError::TrailingBytes.into()),
        std::cmp::Ordering::Equal => {}
    }
    let end = length.checked_sub(32).ok_or(FormatError::InvalidValue)?;
    let (preimage, checksum) = bytes.split_at_checked(end).ok_or(FormatError::Truncated)?;
    let checksum = checksum.try_into().map_err(|_| FormatError::Truncated)?;
    if !crypto.equal_digest(&hash(crypto, preimage)?, &checksum) {
        return Err(Error::Checksum);
    }
    let mut reader = Reader::new(preimage);
    if reader.fixed::<8>()? != *magic
        || reader.u16()? != super::CONTAINER_VERSION
        || reader.u16()? != super::SCHEMA_VERSION
        || reader.u32()? != 0
    {
        return Err(FormatError::InvalidTag.into());
    }
    Ok(reader)
}

fn prefix(writer: &mut Writer<'_>, magic: &[u8; 8]) -> Result<(), FormatError> {
    writer.put(magic)?;
    writer.u16(super::CONTAINER_VERSION)?;
    writer.u16(super::SCHEMA_VERSION)?;
    writer.u32(0)
}

fn publish(crypto: &impl Crypto, scratch: &mut [u8], output: &mut [u8]) -> Result<usize, Error> {
    let length = scratch.len();
    let end = length.checked_sub(32).ok_or(FormatError::InvalidValue)?;
    let (preimage, checksum) = scratch
        .split_at_mut_checked(end)
        .ok_or(FormatError::OutputFull)?;
    let digest = hash(crypto, preimage)?;
    checksum.copy_from_slice(&digest);
    output
        .get_mut(..length)
        .ok_or(FormatError::OutputFull)?
        .copy_from_slice(scratch);
    Ok(length)
}

fn capacity(output: &[u8], length: usize) -> Result<(), FormatError> {
    output
        .get(..length)
        .map(|_| ())
        .ok_or(FormatError::OutputFull)
}

/// Whole-service identity. All 128-bit identity encodings are permitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreIdentity {
    pub instance: InstanceId,
    pub epoch: StoreEpoch,
}
impl StoreIdentity {
    pub fn decode(crypto: &impl Crypto, bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = read(crypto, bytes, super::FORMAT_BYTES, b"TDMTAFMT")?;
        let value = Self {
            instance: InstanceId::from_bytes(reader.fixed()?),
            epoch: StoreEpoch::from_bytes(reader.fixed()?),
        };
        reader.finish()?;
        Ok(value)
    }
    /// Returned errors leave output unchanged; bytes after the container are untouched.
    pub fn encode(self, crypto: &impl Crypto, output: &mut [u8]) -> Result<usize, Error> {
        capacity(output, super::FORMAT_BYTES)?;
        let mut scratch = [0; super::FORMAT_BYTES];
        let mut writer = Writer::new(&mut scratch);
        prefix(&mut writer, b"TDMTAFMT")?;
        writer.put(self.instance.as_bytes())?;
        writer.put(self.epoch.as_bytes())?;
        publish(crypto, &mut scratch, output)
    }
}

/// Checkpoint selection bytes; the referenced manifest still needs validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Current {
    pub account: AccountId,
    pub epoch: StoreEpoch,
    pub generation: u64,
    pub manifest_digest: [u8; 32],
}
impl Current {
    fn validate(self) -> Result<Self, FormatError> {
        if self.generation == 0 {
            return Err(FormatError::InvalidValue);
        }
        Ok(self)
    }
    pub fn decode(crypto: &impl Crypto, bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = read(crypto, bytes, super::CURRENT_BYTES, b"TDMTCUR1")?;
        let value = Self {
            account: AccountId::from_bytes(reader.fixed()?),
            epoch: StoreEpoch::from_bytes(reader.fixed()?),
            generation: reader.u64()?,
            manifest_digest: reader.fixed()?,
        }
        .validate()?;
        reader.finish()?;
        Ok(value)
    }
    /// Returned errors leave output unchanged; bytes after the container are untouched.
    pub fn encode(self, crypto: &impl Crypto, output: &mut [u8]) -> Result<usize, Error> {
        self.validate()?;
        capacity(output, super::CURRENT_BYTES)?;
        let mut scratch = [0; super::CURRENT_BYTES];
        let mut writer = Writer::new(&mut scratch);
        prefix(&mut writer, b"TDMTCUR1")?;
        writer.put(self.account.as_bytes())?;
        writer.put(self.epoch.as_bytes())?;
        writer.u64(self.generation)?;
        writer.put(&self.manifest_digest)?;
        publish(crypto, &mut scratch, output)
    }
}

/// Decode exactly the fixed header, separately from any journal frames.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalHeader {
    pub account: AccountId,
    pub epoch: StoreEpoch,
    pub segment: u64,
    pub base: Sequence,
}
impl JournalHeader {
    fn validate(self) -> Result<Self, FormatError> {
        if self.segment == 0 {
            return Err(FormatError::InvalidValue);
        }
        Ok(self)
    }
    pub fn decode(crypto: &impl Crypto, bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = read(crypto, bytes, super::JOURNAL_HEADER_BYTES, b"TDMTJNL1")?;
        let value = Self {
            account: AccountId::from_bytes(reader.fixed()?),
            epoch: StoreEpoch::from_bytes(reader.fixed()?),
            segment: reader.u64()?,
            base: Sequence::from_u64(reader.u64()?),
        }
        .validate()?;
        reader.finish()?;
        Ok(value)
    }
    /// Returned errors leave output unchanged; bytes after the container are untouched.
    pub fn encode(self, crypto: &impl Crypto, output: &mut [u8]) -> Result<usize, Error> {
        self.validate()?;
        capacity(output, super::JOURNAL_HEADER_BYTES)?;
        let mut scratch = [0; super::JOURNAL_HEADER_BYTES];
        let mut writer = Writer::new(&mut scratch);
        prefix(&mut writer, b"TDMTJNL1")?;
        writer.put(self.account.as_bytes())?;
        writer.put(self.epoch.as_bytes())?;
        writer.u64(self.segment)?;
        writer.u64(self.base.number())?;
        publish(crypto, &mut scratch, output)
    }
}
