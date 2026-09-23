//! Bounded wire identities. Parsing never authorizes a resource or creates a path.
use crate::ids::{AccountId, BlobId, EmailId, IdentityId, MailboxId, SubmissionId, ThreadId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Syntax,
    OutputFull,
    Overflow,
    InvalidRange,
    DepthLimit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Id<'a>(&'a str);
impl<'a> Id<'a> {
    pub fn parse(value: &'a str) -> Result<Self, Error> {
        if value.is_empty()
            || value.len() > 255
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err(Error::Syntax);
        }
        Ok(Self(value))
    }
    pub const fn as_str(self) -> &'a str {
        self.0
    }
    /// Classifies a local blob form, without checking existence or access.
    pub fn blob(self) -> Option<BlobReference> {
        if let Some(id) = self.local::<BlobId>() {
            return Some(BlobReference::File(id));
        }
        if let Ok(part) = PartLocator::decode(self.0) {
            return Some(BlobReference::Part(part));
        }
        NestedLocator::decode(self.0)
            .ok()
            .map(BlobReference::Nested)
    }
    /// A valid foreign-format ID is absent locally, not an invalid argument.
    pub fn local<T: LocalId>(self) -> Option<T> {
        self.local_bytes(T::KIND).map(T::from_storage)
    }
    fn local_bytes(self, kind: IdKind) -> Option<[u8; 16]> {
        let (prefix, digits) = self.0.as_bytes().split_first()?;
        if *prefix != kind.prefix() {
            return None;
        }
        unhex(digits).ok()
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdKind {
    Account,
    Mailbox,
    Email,
    Thread,
    Blob,
    Identity,
    EmailSubmission,
}
impl IdKind {
    pub const WIRE_BYTES: usize = 33;
    pub const fn prefix(self) -> u8 {
        match self {
            Self::Account => b'a',
            Self::Mailbox => b'm',
            Self::Email => b'e',
            Self::Thread => b't',
            Self::Blob => b'b',
            Self::Identity => b'i',
            Self::EmailSubmission => b's',
        }
    }
    fn encode<'a>(self, id: &[u8; 16], output: &'a mut [u8]) -> Result<&'a str, Error> {
        let output = output
            .get_mut(..Self::WIRE_BYTES)
            .ok_or(Error::OutputFull)?;
        let (prefix, digits) = output.split_first_mut().ok_or(Error::OutputFull)?;
        *prefix = self.prefix();
        hex(id, digits)?;
        std::str::from_utf8(output).map_err(|_| Error::Syntax)
    }
}
mod sealed {
    pub trait Local {}
}
/// The sealed roster keeps kind selection attached to the storage ID type.
pub trait LocalId: sealed::Local + Copy {
    const KIND: IdKind;
    fn storage_bytes(&self) -> &[u8; 16];
    fn from_storage(bytes: [u8; 16]) -> Self;
}
macro_rules! local_ids {
    ($($ty:ty => $kind:ident),+ $(,)?) => { $(
        impl sealed::Local for $ty {}
        impl LocalId for $ty {
            const KIND: IdKind = IdKind::$kind;
            fn storage_bytes(&self) -> &[u8; 16] { self.as_bytes() }
            fn from_storage(bytes: [u8; 16]) -> Self { Self::from_bytes(bytes) }
        }
    )+ };
}
local_ids!(AccountId=>Account, MailboxId=>Mailbox, EmailId=>Email,
    ThreadId=>Thread, BlobId=>Blob, IdentityId=>Identity, SubmissionId=>EmailSubmission);
pub fn encode_id<T: LocalId>(id: T, output: &mut [u8]) -> Result<&str, Error> {
    T::KIND.encode(id.storage_bytes(), output)
}
fn digit(value: u8) -> Result<u8, Error> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(Error::Syntax),
    }
}
pub(crate) fn unhex<const N: usize>(bytes: &[u8]) -> Result<[u8; N], Error> {
    if bytes.len() != N.checked_mul(2).ok_or(Error::Overflow)? {
        return Err(Error::Syntax);
    }
    let (pairs, _) = bytes.as_chunks::<2>();
    let mut output = [0; N];
    for (byte, &[high, low]) in output.iter_mut().zip(pairs) {
        *byte = (digit(high)? << 4) | digit(low)?;
    }
    Ok(output)
}
pub(crate) fn hex(bytes: &[u8], output: &mut [u8]) -> Result<(), Error> {
    let output = output
        .get_mut(..bytes.len().checked_mul(2).ok_or(Error::Overflow)?)
        .ok_or(Error::OutputFull)?;
    let (pairs, _) = output.as_chunks_mut::<2>();
    for (byte, [a, b]) in bytes.iter().zip(pairs) {
        let high = byte >> 4;
        let low = byte & 15;
        *a = if high < 10 {
            b'0' + high
        } else {
            b'a' + high - 10
        };
        *b = if low < 10 {
            b'0' + low
        } else {
            b'a' + low - 10
        };
    }
    Ok(())
}

/// Identity is used for unknown transfer encodings under JMAP's decoding rule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferEncoding {
    Identity,
    Base64,
    QuotedPrintable,
}
impl TransferEncoding {
    const fn tag(self) -> u8 {
        match self {
            Self::Identity => 0,
            Self::Base64 => 1,
            Self::QuotedPrintable => 2,
        }
    }
    fn from_tag(tag: u8) -> Result<Self, Error> {
        match tag {
            0 => Ok(Self::Identity),
            1 => Ok(Self::Base64),
            2 => Ok(Self::QuotedPrintable),
            _ => Err(Error::Syntax),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlobReference {
    File(BlobId),
    Part(PartLocator),
    Nested(NestedLocator),
}

/// An untrusted locator until matched against a live authorized parsed part.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PartLocator {
    pub parent: BlobId,
    pub offset: u64,
    pub length: u64,
    pub encoding: TransferEncoding,
}
impl PartLocator {
    pub const WIRE_BYTES: usize = 69;
    pub fn checked_end(self, parent_length: u64) -> Result<u64, Error> {
        let end = self
            .offset
            .checked_add(self.length)
            .ok_or(Error::Overflow)?;
        if end > parent_length {
            return Err(Error::InvalidRange);
        }
        Ok(end)
    }
    pub fn encode(self, output: &mut [u8]) -> Result<&str, Error> {
        self.offset
            .checked_add(self.length)
            .ok_or(Error::Overflow)?;
        let output = output
            .get_mut(..Self::WIRE_BYTES)
            .ok_or(Error::OutputFull)?;
        let (prefix, rest) = output.split_at_mut_checked(3).ok_or(Error::OutputFull)?;
        prefix.copy_from_slice(b"p1_");
        let (parent, rest) = rest.split_at_mut_checked(32).ok_or(Error::OutputFull)?;
        hex(self.parent.as_bytes(), parent)?;
        let (offset, rest) = rest.split_at_mut_checked(16).ok_or(Error::OutputFull)?;
        hex(&self.offset.to_be_bytes(), offset)?;
        let (length, encoding) = rest.split_at_mut_checked(16).ok_or(Error::OutputFull)?;
        hex(&self.length.to_be_bytes(), length)?;
        hex(&[self.encoding.tag()], encoding)?;
        std::str::from_utf8(output).map_err(|_| Error::Syntax)
    }
    pub fn decode(value: &str) -> Result<Self, Error> {
        let value = value.as_bytes();
        if value.len() != Self::WIRE_BYTES {
            return Err(Error::Syntax);
        }
        let (prefix, rest) = value.split_at_checked(3).ok_or(Error::Syntax)?;
        if prefix != b"p1_" {
            return Err(Error::Syntax);
        }
        let (parent, rest) = rest.split_at_checked(32).ok_or(Error::Syntax)?;
        let (offset, rest) = rest.split_at_checked(16).ok_or(Error::Syntax)?;
        let (length, encoding) = rest.split_at_checked(16).ok_or(Error::Syntax)?;
        let [tag] = unhex(encoding)?;
        let locator = Self {
            parent: BlobId::from_bytes(unhex(parent)?),
            offset: u64::from_be_bytes(unhex(offset)?),
            length: u64::from_be_bytes(unhex(length)?),
            encoding: TransferEncoding::from_tag(tag)?,
        };
        locator
            .offset
            .checked_add(locator.length)
            .ok_or(Error::Overflow)?;
        Ok(locator)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Syntax => "invalid wire identifier",
            Self::OutputFull => "wire output capacity exceeded",
            Self::Overflow => "wire range arithmetic overflow",
            Self::InvalidRange => "wire range exceeds parent",
            Self::DepthLimit => "wire locator nesting limit exceeded",
        })
    }
}
impl std::error::Error for Error {}

/// Each range is relative to the preceding stage's decoded octet stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PartStep {
    pub offset: u64,
    pub length: u64,
    pub encoding: TransferEncoding,
}
impl PartStep {
    const EMPTY: Self = Self {
        offset: 0,
        length: 0,
        encoding: TransferEncoding::Identity,
    };
    pub fn checked_end(self, parent_length: u64) -> Result<u64, Error> {
        let end = self
            .offset
            .checked_add(self.length)
            .ok_or(Error::Overflow)?;
        if end > parent_length {
            return Err(Error::InvalidRange);
        }
        Ok(end)
    }
    fn encode(self, output: &mut [u8]) -> Result<(), Error> {
        let (offset, rest) = output.split_at_mut_checked(16).ok_or(Error::OutputFull)?;
        let (length, encoding) = rest.split_at_mut_checked(16).ok_or(Error::OutputFull)?;
        hex(&self.offset.to_be_bytes(), offset)?;
        hex(&self.length.to_be_bytes(), length)?;
        hex(&[self.encoding.tag()], encoding)
    }
    fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let (offset, rest) = bytes.split_at_checked(16).ok_or(Error::Syntax)?;
        let (length, encoding) = rest.split_at_checked(16).ok_or(Error::Syntax)?;
        let [tag] = unhex(encoding)?;
        let value = Self {
            offset: u64::from_be_bytes(unhex(offset)?),
            length: u64::from_be_bytes(unhex(length)?),
            encoding: TransferEncoding::from_tag(tag)?,
        };
        value.checked_end(u64::MAX)?;
        Ok(value)
    }
}

/// Private fixed storage prevents an unchecked count or unbounded nested path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NestedLocator {
    parent: BlobId,
    steps: [PartStep; Self::MAX_STEPS],
    count: usize,
}
impl NestedLocator {
    pub const MAX_STEPS: usize = 6;
    pub const MAX_WIRE_BYTES: usize = 239;
    pub fn new(parent: BlobId, steps: &[PartStep]) -> Result<Self, Error> {
        if steps.len() < 2 {
            return Err(Error::Syntax);
        }
        if steps.len() > Self::MAX_STEPS {
            return Err(Error::DepthLimit);
        }
        for step in steps {
            step.checked_end(u64::MAX)?;
        }
        let mut value = Self {
            parent,
            steps: [PartStep::EMPTY; Self::MAX_STEPS],
            count: steps.len(),
        };
        for (destination, source) in value.steps.iter_mut().zip(steps) {
            *destination = *source;
        }
        Ok(value)
    }
    pub const fn parent(self) -> BlobId {
        self.parent
    }
    pub fn steps(&self) -> impl ExactSizeIterator<Item = &PartStep> {
        self.steps.iter().take(self.count)
    }
    pub fn encoded_len(&self) -> Result<usize, Error> {
        self.count
            .checked_mul(34)
            .and_then(|n| n.checked_add(35))
            .ok_or(Error::Overflow)
    }
    pub fn encode<'a>(&self, output: &'a mut [u8]) -> Result<&'a str, Error> {
        let output = output
            .get_mut(..self.encoded_len()?)
            .ok_or(Error::OutputFull)?;
        let (prefix, rest) = output.split_at_mut_checked(3).ok_or(Error::OutputFull)?;
        prefix.copy_from_slice(b"p2_");
        let (parent, steps) = rest.split_at_mut_checked(32).ok_or(Error::OutputFull)?;
        hex(self.parent.as_bytes(), parent)?;
        let (chunks, _) = steps.as_chunks_mut::<34>();
        for (step, destination) in self.steps().zip(chunks) {
            step.encode(destination)?;
        }
        std::str::from_utf8(output).map_err(|_| Error::Syntax)
    }
    pub fn decode(value: &str) -> Result<Self, Error> {
        let (prefix, rest) = value.as_bytes().split_at_checked(3).ok_or(Error::Syntax)?;
        if prefix != b"p2_" {
            return Err(Error::Syntax);
        }
        let (parent, rest) = rest.split_at_checked(32).ok_or(Error::Syntax)?;
        let (chunks, tail) = rest.as_chunks::<34>();
        if !tail.is_empty() || chunks.len() < 2 {
            return Err(Error::Syntax);
        }
        if chunks.len() > Self::MAX_STEPS {
            return Err(Error::DepthLimit);
        }
        let mut steps = [PartStep::EMPTY; Self::MAX_STEPS];
        for (destination, chunk) in steps.iter_mut().zip(chunks) {
            *destination = PartStep::decode(chunk)?;
        }
        Self::new(
            BlobId::from_bytes(unhex(parent)?),
            steps.get(..chunks.len()).ok_or(Error::Syntax)?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generic_ids_and_local_ids_have_different_grammars() -> Result<(), Error> {
        for valid in ["NIL", "012345", "-", "_", "A", "bother-server"] {
            assert_eq!(Id::parse(valid)?.as_str(), valid);
            assert_eq!(Id::parse(valid)?.local::<EmailId>(), None);
        }
        for invalid in ["", "a/b", "a.b", "a b", "é", "#creation", "a=", "a\n"] {
            assert_eq!(Id::parse(invalid), Err(Error::Syntax));
        }
        let max = "a".repeat(255);
        Id::parse(&max)?;
        assert_eq!(Id::parse(&"a".repeat(256)), Err(Error::Syntax));
        for absent in [
            "e",
            "e000102",
            "e000102030405060708090a0b0c0d0e0f00",
            "e000102030405060708090a0b0c0d0e0z",
        ] {
            assert_eq!(Id::parse(absent)?.local::<EmailId>(), None);
        }
        let upper = "e000102030405060708090A0B0C0D0E0F";
        assert_eq!(Id::parse(upper)?.local::<EmailId>(), None);
        Ok(())
    }
    #[test]
    fn every_local_kind_has_a_literal_prefix_and_exact_width() -> Result<(), Error> {
        let id = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        for (kind, prefix) in [
            (IdKind::Account, "a"),
            (IdKind::Mailbox, "m"),
            (IdKind::Email, "e"),
            (IdKind::Thread, "t"),
            (IdKind::Blob, "b"),
            (IdKind::Identity, "i"),
            (IdKind::EmailSubmission, "s"),
        ] {
            let expected = format!("{prefix}000102030405060708090a0b0c0d0e0f");
            assert_eq!(IdKind::WIRE_BYTES, 33);
            let mut output = [0xa5; IdKind::WIRE_BYTES + 1];
            let encoded = kind.encode(&id, &mut output)?;
            assert_eq!(encoded, expected);
            assert_eq!(Id::parse(encoded)?.local_bytes(kind), Some(id));
            assert_eq!(output.last(), Some(&0xa5));
            for len in 0..IdKind::WIRE_BYTES {
                let mut short = vec![0xa5; len];
                assert_eq!(kind.encode(&id, &mut short), Err(Error::OutputFull));
                assert!(short.iter().all(|b| *b == 0xa5));
            }
        }
        assert_eq!(
            Id::parse("b000102030405060708090a0b0c0d0e0f")?.local::<EmailId>(),
            None
        );
        Ok(())
    }
    #[test]
    fn part_locator_golden_bytes_and_tags() -> Result<(), Error> {
        let parent = BlobId::from_bytes([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
        let golden = "p1_000102030405060708090a0b0c0d0e0f0000000000000102000000000000000301";
        let part = PartLocator {
            parent,
            offset: 258,
            length: 3,
            encoding: TransferEncoding::Base64,
        };
        assert_eq!(golden.len(), PartLocator::WIRE_BYTES);
        let mut output = [0xa5; 70];
        assert_eq!(part.encode(&mut output)?, golden);
        assert_eq!(output.last(), Some(&0xa5));
        assert_eq!(PartLocator::decode(golden)?, part);
        Id::parse(golden)?;
        assert_eq!(Id::parse(golden)?.local::<BlobId>(), None);
        for (encoding, suffix) in [
            (TransferEncoding::Identity, "00"),
            (TransferEncoding::Base64, "01"),
            (TransferEncoding::QuotedPrintable, "02"),
        ] {
            let expected = format!(
                "p1_000102030405060708090a0b0c0d0e0f00000000000001020000000000000003{suffix}"
            );
            let part = PartLocator { encoding, ..part };
            assert_eq!(part.encode(&mut output)?, expected);
            assert_eq!(PartLocator::decode(&expected)?, part);
        }
        for len in 0..PartLocator::WIRE_BYTES {
            assert_eq!(
                PartLocator::decode(golden.get(..len).ok_or(Error::Syntax)?),
                Err(Error::Syntax)
            );
            let mut short = vec![0xa5; len];
            assert_eq!(part.encode(&mut short), Err(Error::OutputFull));
            assert!(short.iter().all(|b| *b == 0xa5));
        }
        assert_eq!(
            PartLocator::decode(&format!("{golden}0")),
            Err(Error::Syntax)
        );
        for invalid in [
            golden.replace("p1_", "p2_"),
            golden.replace("0f", "0F"),
            golden.replace("0301", "0303"),
            golden.replace("0301", "03zz"),
        ] {
            assert_eq!(PartLocator::decode(&invalid), Err(Error::Syntax));
        }
        Ok(())
    }
    #[test]
    fn unsupported_blob_forms_are_valid_but_not_local() -> Result<(), Error> {
        let whole = "b000102030405060708090a0b0c0d0e0f";
        let part = "p1_000102030405060708090a0b0c0d0e0f0000000000000102000000000000000301";
        assert_eq!(
            Id::parse(whole)?.blob(),
            Some(BlobReference::File(BlobId::from_bytes([
                0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15
            ])))
        );
        assert_eq!(
            Id::parse(part)?.blob(),
            Some(BlobReference::Part(PartLocator::decode(part)?))
        );
        for absent in [
            whole.replacen('b', "e", 1),
            part.replace("p1_", "p2_"),
            part.replace("0301", "0303"),
            part.replace("0f", "0F"),
            String::from("provider_blob"),
        ] {
            assert_eq!(Id::parse(&absent)?.blob(), None);
        }
        assert_eq!(
            Id::parse("p1_00000000000000000000000000000000ffffffffffffffff000000000000000100")?
                .blob(),
            None
        );
        Ok(())
    }

    #[test]
    fn locator_range_check_does_not_wrap_or_authorize() -> Result<(), Error> {
        let part = PartLocator {
            parent: BlobId::from_bytes([0; 16]),
            offset: 10,
            length: 5,
            encoding: TransferEncoding::Identity,
        };
        assert_eq!(part.checked_end(15)?, 15);
        assert_eq!(part.checked_end(14), Err(Error::InvalidRange));
        assert_eq!(
            PartLocator {
                offset: 15,
                length: 0,
                ..part
            }
            .checked_end(15)?,
            15
        );
        assert_eq!(
            PartLocator {
                offset: 16,
                length: 0,
                ..part
            }
            .checked_end(15),
            Err(Error::InvalidRange)
        );
        let overflow = PartLocator {
            offset: u64::MAX,
            length: 1,
            ..part
        };
        let mut output = [0xa5; 69];
        assert_eq!(overflow.encode(&mut output), Err(Error::Overflow));
        assert_eq!(output, [0xa5; 69]);
        assert_eq!(overflow.checked_end(u64::MAX), Err(Error::Overflow));
        assert_eq!(
            PartLocator::decode(
                "p1_00000000000000000000000000000000ffffffffffffffff000000000000000100"
            ),
            Err(Error::Overflow)
        );
        let largest = PartLocator {
            offset: u64::MAX,
            length: 0,
            ..part
        };
        let encoded = largest.encode(&mut output)?;
        assert_eq!(PartLocator::decode(encoded)?, largest);
        Ok(())
    }

    #[test]
    fn typed_wire_ids_match_storage_hex_for_all_byte_values() -> Result<(), Error> {
        for byte in 0..=u8::MAX {
            let id = EmailId::from_bytes([byte; 16]);
            let mut output = [0; IdKind::WIRE_BYTES];
            let value = encode_id(id, &mut output)?;
            assert_eq!(value, format!("e{id}"));
            assert_eq!(Id::parse(value)?.local::<EmailId>(), Some(id));
            assert_eq!(Id::parse(value)?.local::<MailboxId>(), None);
        }
        Ok(())
    }

    #[test]
    fn nested_locator_literal_and_per_stage_ranges() -> Result<(), Error> {
        // First decode a base64 attached email at file[100..140], then its
        // QP leaf at decoded-email[10..13]. Values describe offsets only.
        let parent = BlobId::from_bytes([0xab; 16]);
        let steps = [
            PartStep {
                offset: 100,
                length: 40,
                encoding: TransferEncoding::Base64,
            },
            PartStep {
                offset: 10,
                length: 3,
                encoding: TransferEncoding::QuotedPrintable,
            },
        ];
        let golden="p2_abababababababababababababababab0000000000000064000000000000002801000000000000000a000000000000000302";
        let locator = NestedLocator::new(parent, &steps)?;
        assert_eq!(locator.encoded_len()?, 103);
        let mut output = [0xa5; 240];
        assert_eq!(locator.encode(&mut output)?, golden);
        assert_eq!(NestedLocator::decode(golden)?, locator);
        assert_eq!(
            Id::parse(golden)?.blob(),
            Some(BlobReference::Nested(locator))
        );
        assert_eq!(locator.parent(), parent);
        assert_eq!(locator.steps().copied().collect::<Vec<_>>(), steps);
        assert_eq!(steps.first().ok_or(Error::Syntax)?.checked_end(140)?, 140);
        assert_eq!(steps.last().ok_or(Error::Syntax)?.checked_end(13)?, 13);
        assert_eq!(
            steps.last().ok_or(Error::Syntax)?.checked_end(12),
            Err(Error::InvalidRange)
        );
        for len in 0..103 {
            assert!(NestedLocator::decode(golden.get(..len).ok_or(Error::Syntax)?).is_err());
            let mut short = vec![0xa5; len];
            assert_eq!(locator.encode(&mut short), Err(Error::OutputFull));
            assert!(short.iter().all(|&b| b == 0xa5));
        }
        assert_eq!(
            NestedLocator::decode(&format!("{golden}0")),
            Err(Error::Syntax)
        );
        assert_eq!(
            NestedLocator::decode(&golden.replace("p2_", "p3_")),
            Err(Error::Syntax)
        );
        assert_eq!(
            NestedLocator::decode(&golden.replace("0302", "0303")),
            Err(Error::Syntax)
        );
        assert_eq!(
            NestedLocator::decode(&golden.replace("ab", "AB")),
            Err(Error::Syntax)
        );
        assert_eq!(NestedLocator::new(parent, &[]), Err(Error::Syntax));
        assert_eq!(
            NestedLocator::new(parent, steps.get(..1).ok_or(Error::Syntax)?),
            Err(Error::Syntax)
        );
        let overflow = PartStep {
            offset: u64::MAX,
            length: 1,
            encoding: TransferEncoding::Identity,
        };
        assert_eq!(
            NestedLocator::new(parent, &[*steps.first().ok_or(Error::Syntax)?, overflow]),
            Err(Error::Overflow)
        );
        Ok(())
    }

    #[test]
    fn nested_locator_depth_fits_jmap_id_ceiling() -> Result<(), Error> {
        let step = PartStep {
            offset: 0,
            length: 0,
            encoding: TransferEncoding::Identity,
        };
        let parent = BlobId::from_bytes([0; 16]);
        let locator = NestedLocator::new(parent, &[step; 6])?;
        let mut output = [0xa5; NestedLocator::MAX_WIRE_BYTES];
        assert_eq!(locator.encoded_len()?, 239);
        let encoded = locator.encode(&mut output)?;
        assert_eq!(encoded.len(), 239);
        Id::parse(encoded)?;
        assert_eq!(NestedLocator::decode(encoded)?, locator);
        let varied = [
            PartStep {
                offset: 1,
                length: 2,
                encoding: TransferEncoding::Identity,
            },
            PartStep {
                offset: 3,
                length: 4,
                encoding: TransferEncoding::Base64,
            },
            PartStep {
                offset: 5,
                length: 6,
                encoding: TransferEncoding::QuotedPrintable,
            },
            PartStep {
                offset: 7,
                length: 8,
                encoding: TransferEncoding::Identity,
            },
            PartStep {
                offset: 9,
                length: 10,
                encoding: TransferEncoding::Base64,
            },
            PartStep {
                offset: 11,
                length: 12,
                encoding: TransferEncoding::QuotedPrintable,
            },
        ];
        for count in 2..=6 {
            let locator = NestedLocator::new(parent, varied.get(..count).ok_or(Error::Syntax)?)?;
            let encoded = locator.encode(&mut output)?;
            assert_eq!(NestedLocator::decode(encoded)?, locator);
            assert_eq!(
                Id::parse(encoded)?.blob(),
                Some(BlobReference::Nested(locator))
            );
        }
        let encoded = NestedLocator::new(parent, &[step; 6])?
            .encode(&mut output)?
            .to_owned();
        assert_eq!(
            NestedLocator::new(parent, &[step; 7]),
            Err(Error::DepthLimit)
        );
        assert_eq!(
            NestedLocator::decode(&format!("{encoded}0000000000000000000000000000000000")),
            Err(Error::DepthLimit)
        );
        Ok(())
    }
}
