//! Complete supplied frames; transaction references and durable recovery remain separate.
use super::{
    container::{checked_preimage, Error},
    frame_header::Header,
    operation::{self, Operation, Value},
    Error as FormatError, ObjectType, Sequence, FRAME_FOOTER_BYTES, FRAME_HEADER_BYTES,
    OPERATION_HEADER_BYTES,
};
use crate::ports::{Crypto, CryptoError, Digest};

pub(super) const END: &[u8; 8] = b"TDMTEND1";

/// Missing supplied bytes are distinct from malformed complete contents.
/// Only the I/O adapter can establish that missing bytes are at physical EOF.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    Incomplete {
        required: usize,
        supplied: usize,
    },
    /// Includes provider failure, which does not prove on-disk corruption.
    Invalid(Error),
}
impl From<Error> for DecodeError {
    fn from(error: Error) -> Self {
        Self::Invalid(error)
    }
}
impl From<FormatError> for DecodeError {
    fn from(error: FormatError) -> Self {
        Self::Invalid(error.into())
    }
}
impl From<CryptoError> for DecodeError {
    fn from(error: CryptoError) -> Self {
        Self::Invalid(error.into())
    }
}
impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Incomplete { required, supplied } => write!(
                f,
                "incomplete frame: {supplied} of {required} bytes supplied"
            ),
            Self::Invalid(error) => write!(f, "frame validation failed: {error}"),
        }
    }
}
impl std::error::Error for DecodeError {}

pub(super) fn decode_operation(bytes: &[u8]) -> Result<Operation<'_>, FormatError> {
    let operation = Operation::decode(bytes)?;
    if matches!(operation.value(), Value::Change(change) if change.kind == ObjectType::Identity) {
        return Err(FormatError::InvalidValue);
    }
    Ok(operation)
}
fn take_operation<'a>(bytes: &mut &'a [u8]) -> Result<Operation<'a>, FormatError> {
    let prefix = bytes
        .get(..OPERATION_HEADER_BYTES)
        .ok_or(FormatError::Truncated)?;
    let length = operation::extent(prefix)?;
    let (head, tail) = bytes
        .split_at_checked(length)
        .ok_or(FormatError::Truncated)?;
    let operation = decode_operation(head)?;
    *bytes = tail;
    Ok(operation)
}
fn validate_payload(mut bytes: &[u8], count: usize) -> Result<(), FormatError> {
    for _ in 0..count {
        take_operation(&mut bytes)?;
    }
    if !bytes.is_empty() {
        return Err(FormatError::TrailingBytes);
    }
    Ok(())
}

/// All hashes and local operations passed; final-view semantics remain unchecked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Frame<'a> {
    header: Header,
    payload: &'a [u8],
}
/// Binds the checked header to its exact immutable supplied bytes.
pub(super) struct CheckedHeader<'a> {
    header: Header,
    bytes: &'a [u8],
}
impl<'a> CheckedHeader<'a> {
    pub(super) fn read(
        crypto: &impl Crypto,
        previous: Sequence,
        bytes: &'a [u8],
    ) -> Result<Self, DecodeError> {
        let expected = previous.successor()?;
        if bytes.len() < FRAME_HEADER_BYTES {
            return Err(DecodeError::Incomplete {
                required: FRAME_HEADER_BYTES,
                supplied: bytes.len(),
            });
        }
        let header = Header::decode(
            crypto,
            bytes
                .get(..FRAME_HEADER_BYTES)
                .ok_or(FormatError::Truncated)?,
        )?;
        if header.sequence != expected {
            return Err(FormatError::InvalidValue.into());
        }
        Ok(Self { header, bytes })
    }
    pub(super) const fn header(&self) -> Header {
        self.header
    }
    pub(super) fn finish(self, crypto: &impl Crypto) -> Result<Frame<'a>, DecodeError> {
        let header = self.header;
        let bytes = self.bytes;
        if bytes.len() < header.frame_bytes {
            return Err(DecodeError::Incomplete {
                required: header.frame_bytes,
                supplied: bytes.len(),
            });
        }
        let preimage = checked_preimage(crypto, bytes, header.frame_bytes)?;
        let end = preimage
            .len()
            .checked_sub(END.len())
            .ok_or(FormatError::Truncated)?;
        let (body, magic) = preimage
            .split_at_checked(end)
            .ok_or(FormatError::Truncated)?;
        if magic != END {
            return Err(FormatError::InvalidTag.into());
        }
        let payload = body
            .get(FRAME_HEADER_BYTES..)
            .ok_or(FormatError::Truncated)?;
        validate_payload(payload, header.operations)?;
        Ok(Frame { header, payload })
    }
}

impl<'a> Frame<'a> {
    /// Requires the next sequence after `previous`; does not establish physical EOF.
    pub fn decode(
        crypto: &impl Crypto,
        previous: Sequence,
        bytes: &'a [u8],
    ) -> Result<Self, DecodeError> {
        CheckedHeader::read(crypto, previous, bytes)?.finish(crypto)
    }
    pub const fn header(self) -> Header {
        self.header
    }
    pub fn operations(self) -> Operations<'a> {
        Operations {
            remaining: self.payload,
            ordinal: 0,
            failed: false,
        }
    }
}

/// One stored position, including CHANGE descriptors. Ordinals start at zero.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Entry<'a> {
    pub ordinal: usize,
    pub operation: Operation<'a>,
}
/// Defensive reparsing of an immutable, previously validated payload.
pub struct Operations<'a> {
    remaining: &'a [u8],
    ordinal: usize,
    failed: bool,
}
impl<'a> Iterator for Operations<'a> {
    type Item = Result<Entry<'a>, FormatError>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.remaining.is_empty() {
            return None;
        }
        let result = (|| {
            let operation = take_operation(&mut self.remaining)?;
            let ordinal = self.ordinal;
            self.ordinal = ordinal.checked_add(1).ok_or(FormatError::Overflow)?;
            Ok(Entry { ordinal, operation })
        })();
        self.failed = result.is_err();
        Some(result)
    }
}
impl std::iter::FusedIterator for Operations<'_> {}

/// Seal a caller-built payload in an exact frame buffer, preserving all bytes on error.
/// Only 64 header bytes and the footer digest are staged; no frame-sized scratch is used.
pub fn seal<'a>(
    crypto: &impl Crypto,
    sequence: Sequence,
    operations: usize,
    bytes: &'a mut [u8],
) -> Result<Frame<'a>, Error> {
    let header = Header {
        frame_bytes: bytes.len(),
        operations,
        sequence,
    };
    let mut encoded = [0; FRAME_HEADER_BYTES];
    header.encode(crypto, &mut encoded)?;
    let payload_bytes = header.payload_bytes()?;
    let (destination, rest) = bytes
        .split_at_mut_checked(FRAME_HEADER_BYTES)
        .ok_or(FormatError::Truncated)?;
    let (payload, footer) = rest
        .split_at_mut_checked(payload_bytes)
        .ok_or(FormatError::Truncated)?;
    if footer.len() != FRAME_FOOTER_BYTES {
        return Err(FormatError::InvalidValue.into());
    }
    let (magic, checksum) = footer
        .split_at_mut_checked(END.len())
        .ok_or(FormatError::Truncated)?;
    let checksum: &mut [u8; 32] = checksum.try_into().map_err(|_| FormatError::Truncated)?;
    validate_payload(payload, operations)?;
    let mut digest = crypto.sha256()?;
    digest.update(&encoded)?;
    digest.update(payload)?;
    digest.update(END)?;
    let result = digest.finish()?;
    destination.copy_from_slice(&encoded);
    magic.copy_from_slice(END);
    checksum.copy_from_slice(&result);
    Ok(Frame { header, payload })
}
