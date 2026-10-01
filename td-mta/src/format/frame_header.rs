//! Exact transaction headers; a valid header does not validate a frame or its rows.
use super::{
    container::{capacity, prefix, publish, read, Error},
    scalar::Writer,
    Error as FormatError, Sequence, FRAME_FOOTER_BYTES, FRAME_HEADER_BYTES, MAX_FRAME_BYTES,
    MAX_FRAME_OPERATIONS, MAX_KEY_BYTES, MAX_VALUE_BYTES, MIN_FRAME_BYTES, MIN_KEY_BYTES,
    OPERATION_HEADER_BYTES,
};
use crate::ports::Crypto;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    pub frame_bytes: usize,
    pub operations: usize,
    pub sequence: Sequence,
}
impl Header {
    fn validate(self) -> Result<Self, FormatError> {
        if !(MIN_FRAME_BYTES..=MAX_FRAME_BYTES).contains(&self.frame_bytes)
            || !(1..=MAX_FRAME_OPERATIONS).contains(&self.operations)
        {
            return Err(FormatError::Limit);
        }
        if self.sequence.number() == 0 {
            return Err(FormatError::InvalidValue);
        }
        let payload = self.payload_bytes()?;
        let minimum = self
            .operations
            .checked_mul(OPERATION_HEADER_BYTES + MIN_KEY_BYTES)
            .ok_or(FormatError::Overflow)?;
        let maximum = self
            .operations
            .checked_mul(OPERATION_HEADER_BYTES + MAX_KEY_BYTES + MAX_VALUE_BYTES)
            .ok_or(FormatError::Overflow)?;
        if payload < minimum || payload > maximum {
            return Err(FormatError::InvalidValue);
        }
        Ok(self)
    }
    /// Arithmetic only; operation bytes, footer, sequence continuity and I/O remain unchecked.
    pub fn payload_bytes(self) -> Result<usize, FormatError> {
        self.frame_bytes
            .checked_sub(FRAME_HEADER_BYTES + FRAME_FOOTER_BYTES)
            .ok_or(FormatError::InvalidValue)
    }
    /// Hash the fixed 32-byte preimage before interpreting its declared frame length.
    pub fn decode(crypto: &impl Crypto, bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = read(crypto, bytes, FRAME_HEADER_BYTES, b"TDMTFRM1")?;
        let header = Self {
            frame_bytes: usize::try_from(reader.u32()?).map_err(|_| FormatError::Overflow)?,
            operations: usize::try_from(reader.u32()?).map_err(|_| FormatError::Overflow)?,
            sequence: Sequence::from_u64(reader.u64()?),
        };
        reader.finish()?;
        Ok(header.validate()?)
    }
    /// All returned errors preserve output; success preserves its suffix.
    pub fn encode(self, crypto: &impl Crypto, output: &mut [u8]) -> Result<usize, Error> {
        self.validate()?;
        capacity(output, FRAME_HEADER_BYTES)?;
        let mut scratch = [0; FRAME_HEADER_BYTES];
        let mut writer = Writer::new(&mut scratch);
        prefix(&mut writer, b"TDMTFRM1")?;
        writer.u32(u32::try_from(self.frame_bytes).map_err(|_| FormatError::Overflow)?)?;
        writer.u32(u32::try_from(self.operations).map_err(|_| FormatError::Overflow)?)?;
        writer.u64(self.sequence.number())?;
        publish(crypto, &mut scratch, output)
    }
}
