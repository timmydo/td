//! Bounded CTAP canonical CBOR. Strings borrow the caller's owned message.

pub const MAX_BYTES: usize = super::fido_hid::MAX_MESSAGE;
const MAX_ITEMS: usize = 1024;
const MAX_DEPTH: usize = 4;

#[derive(PartialEq)]
pub enum Value<'a> {
    Unsigned(u64),
    Negative(u64),
    Bytes(&'a [u8]),
    Text(&'a str),
    Array(Vec<Value<'a>>),
    Map(Vec<(Value<'a>, Value<'a>)>),
    Simple(u8),
    Float { width: u8, bits: u64 },
}

impl<'a> Value<'a> {
    pub fn unsigned(&self) -> Result<u64, String> {
        match self {
            Self::Unsigned(value) => Ok(*value),
            _ => Err("expected CBOR unsigned integer".into()),
        }
    }

    pub fn bytes(&self) -> Result<&'a [u8], String> {
        match self {
            Self::Bytes(value) => Ok(value),
            _ => Err("expected CBOR byte string".into()),
        }
    }

    pub fn text(&self) -> Result<&'a str, String> {
        match self {
            Self::Text(value) => Ok(value),
            _ => Err("expected CBOR text string".into()),
        }
    }

    pub fn map(&self) -> Result<&[(Self, Self)], String> {
        match self {
            Self::Map(value) => Ok(value),
            _ => Err("expected CBOR map".into()),
        }
    }

    pub fn get(&self, key: &Self) -> Result<Option<&Self>, String> {
        Ok(self
            .map()?
            .iter()
            .find_map(|(candidate, value)| (candidate == key).then_some(value)))
    }

    pub fn required(&self, key: &Self) -> Result<&Self, String> {
        self.get(key)?
            .ok_or_else(|| "missing required CBOR member".into())
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
    items: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or("CBOR length overflow")?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or("truncated CBOR item")?;
        self.offset = end;
        Ok(bytes)
    }

    fn argument(&mut self, additional: u8, shortest: bool) -> Result<u64, String> {
        let (count, minimum) = match additional {
            0..=23 => return Ok(u64::from(additional)),
            24 => (1, 24),
            25 => (2, 256),
            26 => (4, 65536),
            27 => (8, 1u64 << 32),
            _ => return Err("indefinite or reserved CBOR argument".into()),
        };
        let mut value = 0;
        for byte in self.take(count)? {
            value = (value << 8) | u64::from(*byte);
        }
        if shortest && value < minimum {
            return Err("nonminimal CBOR argument".into());
        }
        Ok(value)
    }

    fn value(&mut self, depth: usize) -> Result<Value<'a>, String> {
        if self.items == MAX_ITEMS {
            return Err("CTAP CBOR item limit".into());
        }
        self.items += 1;
        let head = *self.take(1)?.first().ok_or("missing CBOR head")?;
        let major = head >> 5;
        let additional = head & 31;
        if major == 6 {
            return Err("CTAP CBOR tags are forbidden".into());
        }
        let number = self.argument(additional, major != 7 || additional == 24)?;
        match major {
            0 => Ok(Value::Unsigned(number)),
            1 => Ok(Value::Negative(number)),
            2 | 3 => {
                let count = usize::try_from(number).map_err(|_| "CBOR string length overflow")?;
                let bytes = self.take(count)?;
                if major == 2 {
                    Ok(Value::Bytes(bytes))
                } else {
                    Ok(Value::Text(
                        std::str::from_utf8(bytes).map_err(|_| "invalid CBOR UTF-8")?,
                    ))
                }
            }
            4 | 5 => {
                if depth >= MAX_DEPTH {
                    return Err("CTAP CBOR nesting limit".into());
                }
                let count =
                    usize::try_from(number).map_err(|_| "CBOR container length overflow")?;
                let members = count
                    .checked_mul(if major == 5 { 2 } else { 1 })
                    .ok_or("CBOR member overflow")?;
                if members > MAX_ITEMS - self.items || members > self.bytes.len() - self.offset {
                    return Err("CTAP CBOR container limit or truncation".into());
                }
                if major == 4 {
                    let mut values = Vec::with_capacity(count);
                    for _ in 0..count {
                        values.push(self.value(depth + 1)?);
                    }
                    return Ok(Value::Array(values));
                }
                let mut entries = Vec::with_capacity(count);
                let mut previous: Option<&[u8]> = None;
                for _ in 0..count {
                    let start = self.offset;
                    let key = self.value(depth + 1)?;
                    if matches!(key, Value::Array(_) | Value::Map(_)) {
                        return Err("unsupported complex CTAP CBOR map key".into());
                    }
                    let encoded = self
                        .bytes
                        .get(start..self.offset)
                        .ok_or("invalid CBOR key extent")?;
                    if previous.is_some_and(|old| old >= encoded) {
                        return Err("duplicate or unordered CTAP CBOR map key".into());
                    }
                    previous = Some(encoded);
                    entries.push((key, self.value(depth + 1)?));
                }
                Ok(Value::Map(entries))
            }
            7 if additional <= 24 => {
                let simple = u8::try_from(number).map_err(|_| "CBOR simple overflow")?;
                if (24..=31).contains(&simple) {
                    return Err("reserved CBOR simple value".into());
                }
                Ok(Value::Simple(simple))
            }
            7 => Ok(Value::Float {
                width: additional,
                bits: number,
            }),
            _ => Err("invalid CBOR major type".into()),
        }
    }
}

pub fn prefix(bytes: &[u8]) -> Result<(Value<'_>, usize), String> {
    if bytes.len() > MAX_BYTES {
        return Err("CTAP CBOR byte limit".into());
    }
    let mut reader = Reader {
        bytes,
        offset: 0,
        items: 0,
    };
    let value = reader.value(0)?;
    Ok((value, reader.offset))
}

pub fn decode(bytes: &[u8]) -> Result<Value<'_>, String> {
    let (value, count) = prefix(bytes)?;
    if count != bytes.len() {
        return Err("trailing CTAP CBOR bytes".into());
    }
    Ok(value)
}

/// The caller supplies container sizes and order; finish checks the whole value.
pub struct Encoder(Vec<u8>);

impl Drop for Encoder {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

impl Encoder {
    pub fn new() -> Self {
        Self(Vec::with_capacity(MAX_BYTES))
    }

    fn extend(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() > MAX_BYTES - self.0.len() {
            return Err("CTAP CBOR byte limit".into());
        }
        self.0.extend_from_slice(bytes);
        Ok(())
    }

    pub fn head(&mut self, major: u8, value: u64) -> Result<(), String> {
        if major > 5 {
            return Err("unsupported encoded CBOR major type".into());
        }
        let (additional, count) = match value {
            0..=23 => (value as u8, 0),
            24..=255 => (24, 1),
            256..=65535 => (25, 2),
            65536..=4294967295 => (26, 4),
            _ => (27, 8),
        };
        self.extend(&[(major << 5) | additional])?;
        self.extend(
            value
                .to_be_bytes()
                .get(8 - count..)
                .ok_or("CBOR integer extent")?,
        )
    }

    pub fn bytes(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.head(2, bytes.len() as u64)?;
        self.extend(bytes)
    }

    pub fn text(&mut self, text: &str) -> Result<(), String> {
        self.head(3, text.len() as u64)?;
        self.extend(text.as_bytes())
    }

    pub fn boolean(&mut self, value: bool) -> Result<(), String> {
        self.extend(&[if value { 0xf5 } else { 0xf4 }])
    }

    /// Transfers the allocation; the successful caller owns its clearing.
    pub fn finish(mut self) -> Result<Vec<u8>, String> {
        decode(&self.0)?;
        Ok(std::mem::take(&mut self.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_integer_boundaries_and_prefix() {
        for value in [
            0,
            23,
            24,
            255,
            256,
            65535,
            65536,
            u32::MAX as u64,
            1 << 32,
            u64::MAX,
        ] {
            for major in [0, 1] {
                let mut out = Encoder::new();
                out.head(major, value).unwrap();
                let bytes = out.finish().unwrap();
                let parsed = decode(&bytes).unwrap();
                assert!(
                    parsed
                        == if major == 0 {
                            Value::Unsigned(value)
                        } else {
                            Value::Negative(value)
                        }
                );
            }
        }
        assert_eq!(prefix(&[0, 1]).unwrap().1, 1);
        assert!(decode(&[0, 1]).is_err());
        for bytes in [
            &[0x18, 23][..],
            &[0x19, 0, 24],
            &[0x1a, 0, 0, 1, 0],
            &[0x58, 0],
            &[0x9f, 0xff],
            &[0xc0, 0],
            &[0x61, 0xff],
            &[0xf8, 24],
        ] {
            assert!(decode(bytes).is_err(), "{bytes:?}");
        }
        assert!(matches!(
            decode(&[0xf9, 0, 0]).unwrap(),
            Value::Float { width: 25, bits: 0 }
        ));
        assert!(matches!(
            decode(&[0xfa, 0, 0, 0, 0]).unwrap(),
            Value::Float { width: 26, bits: 0 }
        ));
    }

    #[test]
    fn map_order_duplicates_complex_keys_and_limits() {
        assert!(decode(&[0xa2, 0x18, 24, 0, 0x20, 0]).is_ok());
        for bad in [
            vec![0xa2, 1, 0, 1, 1],
            vec![0xa2, 2, 0, 1, 1],
            vec![0xa1, 0x80, 0],
            vec![0xa1, 0xa0, 0],
            vec![0x9b, 255, 255, 255, 255, 255, 255, 255, 255],
        ] {
            assert!(decode(&bad).is_err());
        }
        assert!(decode(&[0x81, 0x81, 0x81, 0x81, 0]).is_ok());
        assert!(decode(&[0x81, 0x81, 0x81, 0x81, 0x80]).is_err());
        let mut too_many = vec![0x99, 4, 0];
        too_many.extend([0; 1024]);
        assert!(decode(&too_many).is_err());
        assert!(decode(&vec![0; MAX_BYTES + 1]).is_err());
        let mut malformed = Encoder::new();
        malformed.head(5, 1).unwrap();
        assert!(malformed.finish().is_err());
    }

    #[test]
    fn every_truncation_is_refused_and_strings_borrow() {
        let mut out = Encoder::new();
        out.head(5, 2).unwrap();
        out.head(0, 1).unwrap();
        out.bytes(&[7; 100]).unwrap();
        out.text("abc").unwrap();
        out.boolean(true).unwrap();
        let bytes = out.finish().unwrap();
        for size in 0..bytes.len() {
            assert!(decode(&bytes[..size]).is_err());
        }
        let parsed = decode(&bytes).unwrap();
        let borrowed = parsed
            .required(&Value::Unsigned(1))
            .unwrap()
            .bytes()
            .unwrap();
        assert!(std::ptr::eq(borrowed.as_ptr(), bytes[4..].as_ptr()));
        assert_eq!(borrowed, &[7; 100]);
    }
}
