//! Bounded PEM syntax. Certificate decoding does not establish identity or trust.
use crate::Error;

const CHAIN_INPUT: usize = 64 * 1024;
const TRUST_INPUT: usize = 128 * 1024;
const KEY_INPUT: usize = 16 * 1024;
/// Capacity sufficient for any certificate returned by PemCertificates.
pub const CERTIFICATE_DER_CAPACITY: usize = 16 * 1024;
const CHAIN_DER: usize = 64 * 1024;

/// A checked, borrowed certificate envelope. This performs no X.509 verification.
/// Construction and decoding allocate no heap storage. Keep the input alive;
/// each decoded certificate is written to a caller-owned buffer on the cold path.
pub struct PemCertificates<'a> {
    remaining: &'a [u8],
    count: usize,
}

impl<'a> PemCertificates<'a> {
    /// Check a nonempty chain: 64 KiB input, eight certificates, 16 KiB DER each.
    pub fn chain(input: &'a [u8]) -> Result<Self, Error> {
        Self::new(input, CHAIN_INPUT, 8, CHAIN_DER)
    }

    /// Check a nonempty explicit bundle: 128 KiB input, 128 certificates,
    /// 16 KiB DER each. This does not parse or admit certificate trust anchors.
    pub fn trust_bundle(input: &'a [u8]) -> Result<Self, Error> {
        Self::new(input, TRUST_INPUT, 128, TRUST_INPUT)
    }

    fn new(
        input: &'a [u8],
        max_input: usize,
        max_count: usize,
        max_der: usize,
    ) -> Result<Self, Error> {
        if input.len() > max_input {
            return Err(Error::Invalid);
        }
        let mut remaining = whitespace(input);
        let mut count = 0usize;
        let mut total = 0usize;
        while !remaining.is_empty() {
            let block = block(remaining, Label::Certificate)?;
            let decoded = inspect(block.body)?;
            if decoded.length > CERTIFICATE_DER_CAPACITY {
                return Err(Error::Invalid);
            }
            decoded.sequence()?;
            count = count.checked_add(1).ok_or(Error::Invalid)?;
            total = total.checked_add(decoded.length).ok_or(Error::Invalid)?;
            if count > max_count || total > max_der {
                return Err(Error::Invalid);
            }
            remaining = block.remaining;
        }
        if count == 0 {
            return Err(Error::Invalid);
        }
        Ok(Self {
            remaining: whitespace(input),
            count,
        })
    }

    /// Number of certificates not yet decoded.
    pub fn remaining(&self) -> usize {
        self.count
    }

    /// Decode one certificate, preserving the unused output tail.
    /// None means exhaustion and leaves output untouched. A short buffer returns
    /// Capacity without changing output or advancing, so the caller can retry.
    /// Every returned error leaves both output and this reader unchanged.
    pub fn decode_next(&mut self, output: &mut [u8]) -> Result<Option<usize>, Error> {
        if self.count == 0 {
            return Ok(None);
        }
        let block = block(self.remaining, Label::Certificate)?;
        let length = decode(block.body, output)?;
        self.remaining = block.remaining;
        self.count -= 1;
        Ok(Some(length))
    }
}

impl std::fmt::Debug for PemCertificates<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PemCertificates")
            .field("remaining", &self.count)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
enum Label {
    Certificate,
    PrivateKey,
}

impl Label {
    fn markers(self) -> (&'static [u8], &'static [u8]) {
        match self {
            Self::Certificate => (b"-----BEGIN CERTIFICATE-----", b"-----END CERTIFICATE-----"),
            Self::PrivateKey => (b"-----BEGIN PRIVATE KEY-----", b"-----END PRIVATE KEY-----"),
        }
    }
}

fn whitespace(mut input: &[u8]) -> &[u8] {
    while let Some((&byte, rest)) = input.split_first() {
        if !matches!(byte, 9..=13 | 32) {
            break;
        }
        input = rest;
    }
    input
}

fn line(input: &[u8]) -> Result<(&[u8], &[u8]), Error> {
    match input.iter().position(|&b| b == b'\n') {
        Some(end) => {
            let line = input.get(..end).ok_or(Error::Invalid)?;
            let rest = input
                .get(end.checked_add(1).ok_or(Error::Invalid)?..)
                .ok_or(Error::Invalid)?;
            Ok((line.strip_suffix(b"\r").unwrap_or(line), rest))
        }
        None => Ok((input, &[])),
    }
}

struct Block<'a> {
    body: &'a [u8],
    remaining: &'a [u8],
}

fn block(input: &[u8], label: Label) -> Result<Block<'_>, Error> {
    let (begin, end) = label.markers();
    let (header, body) = line(input)?;
    if header != begin || body.is_empty() {
        return Err(Error::Invalid);
    }
    let mut cursor = body;
    while !cursor.is_empty() {
        let (candidate, rest) = line(cursor)?;
        if candidate == end {
            let length = body.len().checked_sub(cursor.len()).ok_or(Error::Invalid)?;
            return Ok(Block {
                body: body.get(..length).ok_or(Error::Invalid)?,
                remaining: whitespace(rest),
            });
        }
        cursor = rest;
    }
    Err(Error::Invalid)
}

fn sextet(byte: u8) -> Result<u8, Error> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(Error::Invalid),
    }
}

// Validate line endings before allowing CR as base64 whitespace.
fn base64(
    input: &[u8],
    mut consume: impl FnMut(&[u8]) -> Result<(), Error>,
) -> Result<usize, Error> {
    let mut previous_cr = false;
    for &byte in input {
        if previous_cr && byte != b'\n' {
            return Err(Error::Invalid);
        }
        previous_cr = byte == b'\r';
    }
    if previous_cr {
        return Err(Error::Invalid);
    }
    let mut input = input
        .iter()
        .copied()
        .filter(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'));
    let mut length = 0usize;
    while let Some(first) = input.next() {
        let a = sextet(first)?;
        let b = sextet(input.next().ok_or(Error::Invalid)?)?;
        let third = input.next().ok_or(Error::Invalid)?;
        let fourth = input.next().ok_or(Error::Invalid)?;
        let (c, d, count) = match (third, fourth) {
            (b'=', b'=') if b & 15 == 0 => (0, 0, 1),
            (b'=', _) => return Err(Error::Invalid),
            (_, b'=') => {
                let c = sextet(third)?;
                if c & 3 != 0 {
                    return Err(Error::Invalid);
                }
                (c, 0, 2)
            }
            _ => (sextet(third)?, sextet(fourth)?, 3),
        };
        let decoded = [(a << 2) | (b >> 4), (b << 4) | (c >> 2), (c << 6) | d];
        length = length.checked_add(count).ok_or(Error::Invalid)?;
        consume(decoded.get(..count).ok_or(Error::Invalid)?)?;
        if count != 3 && input.next().is_some() {
            return Err(Error::Invalid);
        }
    }
    if length == 0 {
        return Err(Error::Invalid);
    }
    Ok(length)
}

struct Decoded {
    length: usize,
    prefix: [u8; 4],
}

impl Decoded {
    // One complete minimally encoded DER SEQUENCE; its fields are not verified.
    fn sequence(&self) -> Result<(), Error> {
        let [tag, first, second, third] = self.prefix;
        if tag != 0x30 {
            return Err(Error::Invalid);
        }
        let (header, content) = match first {
            1..=127 => (2, usize::from(first)),
            0x81 if second >= 128 => (3, usize::from(second)),
            0x82 if second != 0 => (4, usize::from(u16::from_be_bytes([second, third]))),
            _ => return Err(Error::Invalid),
        };
        if header + content != self.length {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}

fn inspect(input: &[u8]) -> Result<Decoded, Error> {
    let mut prefix = [0; 4];
    let mut destination = prefix.iter_mut();
    let length = base64(input, |bytes| {
        for &byte in bytes {
            if let Some(slot) = destination.next() {
                *slot = byte;
            }
        }
        Ok(())
    })?;
    Ok(Decoded { length, prefix })
}

fn decode(input: &[u8], output: &mut [u8]) -> Result<usize, Error> {
    let length = inspect(input)?.length;
    if length > output.len() {
        return Err(Error::Capacity);
    }
    let mut output = output.iter_mut();
    // Immutable input was fully validated before exposing any output.
    base64(input, |bytes| {
        for &byte in bytes {
            *output.next().ok_or(Error::Capacity)? = byte;
        }
        Ok(())
    })
}

struct Secret([u8; crate::P256_PKCS8_CAPACITY]);
impl Secret {
    fn clear(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&mut self.0);
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.clear();
    }
}

pub(super) fn private_key<T>(
    input: &[u8],
    use_key: impl FnOnce(&[u8]) -> Result<T, Error>,
) -> Result<T, Error> {
    if input.len() > KEY_INPUT {
        return Err(Error::Invalid);
    }
    let block = block(whitespace(input), Label::PrivateKey)?;
    if !block.remaining.is_empty() {
        return Err(Error::Invalid);
    }
    let mut secret = Secret([0; crate::P256_PKCS8_CAPACITY]);
    let length = decode(block.body, &mut secret.0).map_err(|error| match error {
        Error::Capacity => Error::Invalid,
        other => other,
    })?;
    let der = secret.0.get(..length).ok_or(Error::Invalid)?;
    crate::pkcs8::validate(der)?;
    use_key(der)
}

#[cfg(test)]
#[path = "pem_tests.rs"]
pub(crate) mod tests;
