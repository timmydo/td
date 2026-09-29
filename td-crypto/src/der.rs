//! Small definite-length DER reader for bounded certificate metadata.
use crate::TlsError;

#[derive(Clone, Copy)]
pub(super) struct Element<'a> {
    pub tag: u8,
    pub encoded: &'a [u8],
    pub value: &'a [u8],
}

pub(super) struct Der<'a>(pub &'a [u8]);

impl<'a> Der<'a> {
    pub fn byte(&mut self) -> Result<u8, TlsError> {
        let (&byte, rest) = self.0.split_first().ok_or(TlsError::Invalid)?;
        self.0 = rest;
        Ok(byte)
    }

    pub fn next(&mut self) -> Result<Element<'a>, TlsError> {
        let start = self.0;
        let tag = self.byte()?;
        // Certificate fields used here have only single-octet tags.
        if tag & 31 == 31 {
            return Err(TlsError::Invalid);
        }
        let first = self.byte()?;
        let length = match first {
            0..=127 => usize::from(first),
            0x81 => {
                let n = self.byte()?;
                if n < 128 {
                    return Err(TlsError::Invalid);
                }
                usize::from(n)
            }
            0x82 => {
                let high = self.byte()?;
                let low = self.byte()?;
                if high == 0 {
                    return Err(TlsError::Invalid);
                }
                usize::from(u16::from_be_bytes([high, low]))
            }
            _ => return Err(TlsError::Invalid),
        };
        let value = self.0.get(..length).ok_or(TlsError::Invalid)?;
        self.0 = self.0.get(length..).ok_or(TlsError::Invalid)?;
        let used = start
            .len()
            .checked_sub(self.0.len())
            .ok_or(TlsError::Invalid)?;
        Ok(Element {
            tag,
            encoded: start.get(..used).ok_or(TlsError::Invalid)?,
            value,
        })
    }

    pub fn take(&mut self, tag: u8) -> Result<Element<'a>, TlsError> {
        let element = self.next()?;
        if element.tag != tag {
            return Err(TlsError::Invalid);
        }
        Ok(element)
    }

    pub fn end(self) -> Result<(), TlsError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(TlsError::Invalid)
        }
    }
}

pub(super) fn bit_string(value: &[u8]) -> Result<(&[u8], u8), TlsError> {
    let (&unused, bits) = value.split_first().ok_or(TlsError::Invalid)?;
    if unused > 7 || bits.is_empty() {
        return Err(TlsError::Invalid);
    }
    let last = bits.last().copied().ok_or(TlsError::Invalid)?;
    if last & ((1u8 << unused) - 1) != 0 {
        return Err(TlsError::Invalid);
    }
    Ok((bits, unused))
}

pub(super) fn oid(value: &[u8]) -> Result<(), TlsError> {
    if value.is_empty() {
        return Err(TlsError::Invalid);
    }
    let mut start = true;
    for &byte in value {
        if start && byte == 0x80 {
            return Err(TlsError::Invalid);
        }
        start = byte & 0x80 == 0;
    }
    if start {
        Ok(())
    } else {
        Err(TlsError::Invalid)
    }
}

pub(super) fn utc(element: Element<'_>) -> Result<u64, TlsError> {
    let expected = match element.tag {
        0x17 => 13,
        0x18 => 15,
        _ => return Err(TlsError::Invalid),
    };
    if element.value.len() != expected || element.value.last() != Some(&b'Z') {
        return Err(TlsError::Invalid);
    }
    let mut bytes = element.value.iter().copied();
    let mut pair = || -> Result<u64, TlsError> {
        let a = bytes.next().ok_or(TlsError::Invalid)?;
        let b = bytes.next().ok_or(TlsError::Invalid)?;
        if !a.is_ascii_digit() || !b.is_ascii_digit() {
            return Err(TlsError::Invalid);
        }
        Ok(u64::from(a - b'0') * 10 + u64::from(b - b'0'))
    };
    let year = if element.tag == 0x17 {
        let short = pair()?;
        if short < 50 {
            2000 + short
        } else {
            1900 + short
        }
    } else {
        pair()? * 100 + pair()?
    };
    let month = pair()?;
    let day = pair()?;
    let hour = pair()?;
    let minute = pair()?;
    let second = pair()?;
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(TlsError::Invalid);
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day == 0 || day > month_days {
        return Err(TlsError::Invalid);
    }
    // Every operand is bounded by a four-digit calendar year.
    let prior = year - 1;
    let before_year = 365 * prior + prior / 4 - prior / 100 + prior / 400;
    let mut days = before_year - 719162;
    for previous in 1..month {
        days += match previous {
            2 if leap => 29,
            2 => 28,
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
    }
    days += day - 1;
    Ok(days * 86400 + hour * 3600 + minute * 60 + second)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn certificate_der_lengths_bit_strings_and_oids() {
        for encoded in [
            &b"\x30\x80\0\0"[..],
            b"\x30\x81\x01\0",
            b"\x30\x82\0\x80",
            b"\x30\x83\0\x01\0",
            b"\x1f\0",
        ] {
            assert!(Der(encoded).next().is_err());
        }
        let mut encoded = vec![0x30, 0x82, 1, 0];
        encoded.resize(260, 0xa5);
        for length in 0..encoded.len() {
            assert!(Der(&encoded[..length]).next().is_err());
        }
        let mut reader = Der(&encoded);
        let element = reader.take(0x30).unwrap();
        assert_eq!(element.encoded, encoded);
        assert_eq!(element.value, &[0xa5; 256]);
        assert!(reader.end().is_ok());
        for value in [&b""[..], &[8, 0], &[1, 1], &[0]] {
            assert!(bit_string(value).is_err());
        }
        assert_eq!(bit_string(&[7, 0x80]), Ok((&[0x80][..], 7)));
        for value in [&b""[..], &[0x80, 0], &[0x2a, 0x80, 1], &[0x2a, 0x81]] {
            assert!(oid(value).is_err());
        }
        for value in [&[0][..], &[0x2a], &[0x81, 0], &[0x2a, 0x81, 1]] {
            assert_eq!(oid(value), Ok(()));
        }
    }

    #[test]
    fn certificate_calendar_boundaries() {
        for (tag, value, seconds) in [
            (0x17, &b"700101000000Z"[..], 0),
            (0x17, b"000229000000Z", 951782400),
            (0x17, b"250101000000Z", 1735689600),
            (0x17, b"350101000000Z", 2051222400),
            (0x18, b"20500101000000Z", 2524608000),
            (0x18, b"99991231235959Z", 253402300799),
        ] {
            assert_eq!(
                utc(Element {
                    tag,
                    encoded: &[],
                    value
                }),
                Ok(seconds)
            );
        }
        for (tag, value) in [
            (0x17, &b"500101000000Z"[..]),
            (0x17, b"700101000000"),
            (0x17, b"250229000000Z"),
            (0x17, b"250101240000Z"),
            (0x17, b"250101006000Z"),
            (0x17, b"250101000060Z"),
            (0x17, b"251301000000Z"),
            (0x17, b"250100000000Z"),
            (0x17, b"250101000000+"),
            (0x17, b"2x0101000000Z"),
            (0x18, b"21000229000000Z"),
            (0x18, b"20260101000000.0Z"),
            (0x18, b"19691231235959Z"),
            (0x16, b"250101000000Z"),
        ] {
            assert_eq!(
                utc(Element {
                    tag,
                    encoded: &[],
                    value
                }),
                Err(TlsError::Invalid)
            );
        }
    }
}
