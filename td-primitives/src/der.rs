//! The strict DER subset signatures and RSA keys need: one definite minimal
//! length and non-negative minimal INTEGERs. Anything else is refused.

use crate::Refused;

const SEQUENCE: u8 = 0x30;
const INTEGER: u8 = 0x02;

/// Split one element with `tag` off the front, returning its value and the rest.
fn element(input: &[u8], tag: u8) -> Result<(&[u8], &[u8]), Refused> {
    let (&found, rest) = input.split_first().ok_or(Refused)?;
    if found != tag {
        return Err(Refused);
    }
    let (&first, rest) = rest.split_first().ok_or(Refused)?;
    let (length, rest) = match first {
        0..=0x7f => (usize::from(first), rest),
        0x81 => {
            let (&low, rest) = rest.split_first().ok_or(Refused)?;
            if low < 0x80 {
                return Err(Refused);
            }
            (usize::from(low), rest)
        }
        0x82 => {
            let (&[high, low], rest) = rest.split_first_chunk::<2>().ok_or(Refused)?;
            let length = usize::from(u16::from_be_bytes([high, low]));
            if length < 0x100 {
                return Err(Refused);
            }
            (length, rest)
        }
        _ => return Err(Refused),
    };
    rest.split_at_checked(length).ok_or(Refused)
}

/// A non-negative INTEGER's value without its sign byte.
fn integer(input: &[u8]) -> Result<(&[u8], &[u8]), Refused> {
    let (value, rest) = element(input, INTEGER)?;
    let value = match value {
        [] => return Err(Refused),
        [high, ..] if high & 0x80 != 0 => return Err(Refused),
        [0, next, ..] if next & 0x80 == 0 => return Err(Refused),
        [0, tail @ ..] if !tail.is_empty() => tail,
        whole => whole,
    };
    Ok((value, rest))
}

/// `SEQUENCE { INTEGER, INTEGER }` with nothing after it: an ECDSA signature
/// or a PKCS#1 RSA public key.
pub(crate) fn two_integers(input: &[u8]) -> Result<(&[u8], &[u8]), Refused> {
    let (body, trailing) = element(input, SEQUENCE)?;
    if !trailing.is_empty() {
        return Err(Refused);
    }
    let (first, rest) = integer(body)?;
    let (second, rest) = integer(rest)?;
    if !rest.is_empty() {
        return Err(Refused);
    }
    Ok((first, second))
}

/// `SEQUENCE { r, s }`, when its body fits the 127 bytes of the short length form.
pub(crate) fn encode_two_integers(a: &[u8], b: &[u8]) -> Result<Vec<u8>, Refused> {
    fn push(out: &mut Vec<u8>, value: &[u8]) -> Result<(), Refused> {
        let start = value.iter().take_while(|byte| **byte == 0).count();
        let trimmed = value.get(start..).unwrap_or_default();
        let trimmed = if trimmed.is_empty() {
            value
                .get(value.len().saturating_sub(1)..)
                .unwrap_or_default()
        } else {
            trimmed
        };
        let sign = trimmed.first().is_some_and(|high| high & 0x80 != 0);
        let length = u8::try_from(trimmed.len() + usize::from(sign)).map_err(|_| Refused)?;
        if length > 0x7f {
            return Err(Refused);
        }
        out.extend_from_slice(&[INTEGER, length]);
        if sign {
            out.push(0);
        }
        out.extend_from_slice(trimmed);
        Ok(())
    }
    let mut body = Vec::with_capacity(a.len() + b.len() + 6);
    push(&mut body, a)?;
    push(&mut body, b)?;
    // The one caller signs on P-256, whose body is at most 70 bytes.
    let length = u8::try_from(body.len()).map_err(|_| Refused)?;
    if length > 0x7f {
        return Err(Refused);
    }
    let mut out = vec![SEQUENCE, length];
    out.extend_from_slice(&body);
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{encode_two_integers, two_integers};

    #[test]
    fn a_body_past_the_short_length_form_is_refused_rather_than_misencoded() {
        assert!(encode_two_integers(&[0x7f; 60], &[0x7f; 60]).is_ok());
        assert!(encode_two_integers(&[0x7f; 70], &[0x7f; 70]).is_err());
    }

    #[test]
    fn the_strict_form_is_read_and_written_back() {
        let der = [0x30, 0x07, 0x02, 0x01, 0x05, 0x02, 0x02, 0x00, 0x80];
        assert_eq!(two_integers(&der).unwrap(), (&[5u8][..], &[0x80u8][..]));
        assert_eq!(encode_two_integers(&[0, 5], &[0x80]).unwrap(), der);
        assert_eq!(
            encode_two_integers(&[0, 0], &[1]).unwrap(),
            [0x30, 0x06, 0x02, 0x01, 0x00, 0x02, 0x01, 0x01]
        );
    }

    #[test]
    fn anything_else_is_refused() {
        for bad in [
            &[][..],
            // Trailing bytes after the sequence and inside it.
            &[0x30, 0x06, 0x02, 0x01, 0x05, 0x02, 0x01, 0x05, 0x00],
            &[0x30, 0x07, 0x02, 0x01, 0x05, 0x02, 0x01, 0x05, 0x00],
            // A long-form length that fits the short form.
            &[0x30, 0x81, 0x06, 0x02, 0x01, 0x05, 0x02, 0x01, 0x05],
            // Indefinite length.
            &[0x30, 0x80, 0x02, 0x01, 0x05, 0x02, 0x01, 0x05, 0x00, 0x00],
            // Negative, needless leading zero, and empty integers.
            &[0x30, 0x06, 0x02, 0x01, 0x85, 0x02, 0x01, 0x05],
            &[0x30, 0x07, 0x02, 0x02, 0x00, 0x05, 0x02, 0x01, 0x05],
            &[0x30, 0x05, 0x02, 0x00, 0x02, 0x01, 0x05],
            // A length running past the input, a wrong tag, one integer.
            &[0x30, 0x7f, 0x02, 0x01, 0x05],
            &[0x31, 0x06, 0x02, 0x01, 0x05, 0x02, 0x01, 0x05],
            &[0x30, 0x03, 0x02, 0x01, 0x05],
        ] {
            assert!(two_integers(bad).is_err(), "{bad:02x?}");
        }
    }
}
