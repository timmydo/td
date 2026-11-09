use super::Error;
const MAX_BYTES: usize = 65536;

pub fn token(value: &str, minimum: usize, maximum: usize) -> Result<(), Error> {
    if value.len() < minimum
        || value.len() > maximum
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(Error::Invalid);
    }
    Ok(())
}

pub fn base64(bytes: &[u8], url: bool) -> Result<String, Error> {
    if bytes.len() > MAX_BYTES {
        return Err(Error::Capacity);
    }
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for part in bytes.chunks(3) {
        let a = u32::from(*part.first().ok_or(Error::Invalid)?);
        let b = u32::from(part.get(1).copied().unwrap_or(0));
        let c = u32::from(part.get(2).copied().unwrap_or(0));
        let word = a << 16 | b << 8 | c;
        for index in 0..4 {
            if index < part.len() + 1 {
                let value = ((word >> (18 - index * 6)) & 63) as u8;
                output.push(char::from(symbol(value, url)));
            } else if !url {
                output.push('=');
            }
        }
    }
    Ok(output)
}

/// The base64 character for a sextet, with no branch and no table: this
/// encodes the generated account and certificate private keys, so a
/// secret value must not steer control flow or memory access. Each range
/// test is an arithmetic sign mask; `url` is public.
fn symbol(value: u8, url: bool) -> u8 {
    let (c62, c63) = if url { (b'-', b'_') } else { (b'+', b'/') };
    let v = i16::from(value & 63);
    // -1 when v < n, else 0.
    let below = |n: i16| (v - n) >> 15;
    let upper = below(26);
    let lower = !upper & below(52);
    let digit = !below(52) & below(62);
    let s62 = !below(62) & below(63);
    let s63 = !below(63);
    let out = (upper & (v + 65))
        | (lower & (v + 71))
        | (digit & (v - 4))
        | (s62 & i16::from(c62))
        | (s63 & i16::from(c63));
    (out & 0xff) as u8
}

pub fn pem(label: &str, bytes: &[u8]) -> Result<Vec<u8>, Error> {
    if !matches!(label, "PRIVATE KEY" | "CERTIFICATE REQUEST") {
        return Err(Error::Invalid);
    }
    let encoded = base64(bytes, false)?;
    let mut output = format!("-----BEGIN {label}-----\n").into_bytes();
    for line in encoded.as_bytes().chunks(64) {
        output.extend_from_slice(line);
        output.push(b'\n');
    }
    output.extend_from_slice(format!("-----END {label}-----\n").as_bytes());
    Ok(output)
}

pub(super) fn der(tag: u8, bytes: &[u8]) -> Result<Vec<u8>, Error> {
    if bytes.len() > 16384 {
        return Err(Error::Capacity);
    }
    let mut output = Vec::with_capacity(bytes.len() + 4);
    output.push(tag);
    if bytes.len() < 128 {
        output.push(bytes.len() as u8);
    } else if bytes.len() < 256 {
        output.extend_from_slice(&[0x81, bytes.len() as u8]);
    } else {
        output.push(0x82);
        output.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    }
    output.extend_from_slice(bytes);
    Ok(output)
}

pub(super) fn signature_der(signature: &[u8; 64]) -> Result<Vec<u8>, Error> {
    let mut integers = Vec::with_capacity(70);
    for scalar in signature.chunks(32) {
        let first = scalar.iter().position(|v| *v != 0).ok_or(Error::Crypto)?;
        let significant = scalar.get(first..).ok_or(Error::Invalid)?;
        let mut value = Vec::with_capacity(33);
        if significant.first().is_some_and(|b| b & 0x80 != 0) {
            value.push(0);
        }
        value.extend_from_slice(significant);
        integers.extend(der(2, &value)?);
    }
    der(0x30, &integers)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn base64_known_answers_and_bounds() -> Result<(), Error> {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes(), false)?, expected);
            assert_eq!(
                base64(input.as_bytes(), true)?,
                expected.trim_end_matches('=')
            );
        }
        assert_eq!(base64(&[251, 255, 255], true)?, "-___");
        assert_eq!(base64(&vec![0; MAX_BYTES + 1], true), Err(Error::Capacity));
        assert!(token("with=padding", 1, 512).is_err());
        assert!(token("with\nnewline", 1, 512).is_err());
        Ok(())
    }
    #[test]
    fn branch_free_symbols_match_the_alphabets() -> Result<(), Error> {
        let standard = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let url = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        for value in 0..64u8 {
            assert_eq!(
                Some(&symbol(value, false)),
                standard.get(usize::from(value))
            );
            assert_eq!(Some(&symbol(value, true)), url.get(usize::from(value)));
        }
        // Differential against the table encoder over every length class.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for length in 0..200usize {
            let bytes: Vec<u8> = (0..length)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect();
            for (alphabet, is_url) in [(standard, false), (url, true)] {
                let mut expected = String::new();
                for part in bytes.chunks(3) {
                    let word = part
                        .iter()
                        .chain([0, 0].iter())
                        .take(3)
                        .fold(0u32, |w, &b| w << 8 | u32::from(b));
                    for index in 0..=part.len() {
                        let value = (word >> (18 - index * 6)) & 63;
                        let symbol = alphabet.get(value as usize).ok_or(Error::Invalid)?;
                        expected.push(char::from(*symbol));
                    }
                    if !is_url {
                        expected.push_str("==".get(..3 - part.len()).ok_or(Error::Invalid)?);
                    }
                }
                assert_eq!(base64(&bytes, is_url)?, expected);
            }
        }
        Ok(())
    }
    #[test]
    fn der_lengths_and_sign_padding() -> Result<(), Error> {
        assert_eq!(der(4, &[0; 128])?.get(..3), Some([4, 0x81, 128].as_slice()));
        assert_eq!(
            der(4, &[0; 256])?.get(..4),
            Some([4, 0x82, 1, 0].as_slice())
        );
        let signature = signature_der(&[0x80; 64])?;
        assert_eq!(signature.get(..5), Some([0x30, 70, 2, 33, 0].as_slice()));
        assert!(signature_der(&[0; 64]).is_err());
        Ok(())
    }
}
