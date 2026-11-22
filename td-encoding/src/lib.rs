//! Byte encodings td's crates each kept a copy of: lowercase hexadecimal
//! both ways, and base64 (RFC 4648 §4, the standard alphabet with `=`
//! padding) both ways. Base64's `_into` forms write to a caller's buffer
//! and allocate nothing, for callers whose growth goes through td-alloc.
//!
//! One file and `std` alone, so a crate built by a direct rustc can include
//! it as a module by `#[path]`, as td-txt does td-regex. Decoders that
//! must tolerate whitespace, either case, or a streaming MIME body keep
//! their own rules where those rules are the protocol's.

#![forbid(unsafe_code)]

const HEX: &[u8; 16] = b"0123456789abcdef";
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `bytes` as lowercase hexadecimal, two digits a byte.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        out.push(digit(HEX, byte >> 4));
        out.push(digit(HEX, byte & 0x0f));
    }
    out
}

/// The bytes lowercase hexadecimal `text` spells, or `None` for an odd
/// length or any character outside `0-9a-f`: uppercase, whitespace and
/// signs included, so one value has one spelling.
pub fn from_hex(text: &str) -> Option<Vec<u8>> {
    let (pairs, odd) = text.as_bytes().as_chunks::<2>();
    if !odd.is_empty() {
        return None;
    }
    pairs
        .iter()
        .map(|[high, low]| Some((nibble(*high)? << 4) | nibble(*low)?))
        .collect()
}

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// The standard base64 encoding of `bytes`, padded.
pub fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3).saturating_mul(4));
    encode_base64(bytes, |symbol| out.push(char::from(symbol)));
    out
}

/// `base64(bytes)` written to the front of `out`, allocating nothing:
/// the length written, or `None`, writing nothing, when `out` is shorter
/// than the encoding.
pub fn base64_into(bytes: &[u8], out: &mut [u8]) -> Option<usize> {
    let length = bytes.len().div_ceil(3).checked_mul(4)?;
    let mut slots = out.get_mut(..length)?.iter_mut();
    encode_base64(bytes, |symbol| {
        if let Some(slot) = slots.next() {
            *slot = symbol;
        }
    });
    Some(length)
}

/// Every symbol of the padded standard base64 of `bytes`, in order.
fn encode_base64(bytes: &[u8], mut emit: impl FnMut(u8)) {
    for chunk in bytes.chunks(3) {
        let a = chunk.first().copied().unwrap_or(0);
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        let sextets = [
            a >> 2,
            ((a & 3) << 4) | (b >> 4),
            ((b & 15) << 2) | (c >> 6),
            c & 63,
        ];
        // A chunk of n bytes carries n + 1 sextets; padding fills the rest.
        for (index, sextet) in sextets.into_iter().enumerate() {
            emit(if index > chunk.len() {
                b'='
            } else {
                symbol(BASE64, sextet)
            });
        }
    }
}

/// The bytes the standard padded base64 `text` spells, or `None` for any
/// other spelling: a length not a multiple of four, padding missing,
/// misplaced or past two, a symbol outside the standard alphabet
/// (whitespace and the URL-safe `-_` included), or bits the padding drops
/// that are not zero, so one value has one spelling.
pub fn from_base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity((text.len() / 4).saturating_mul(3));
    decode_base64(text.as_bytes(), |bytes| {
        out.extend_from_slice(bytes);
        Some(())
    })?;
    Some(out)
}

/// `from_base64(text)` written to the front of `out`, allocating nothing:
/// the length written, or `None` for any spelling `from_base64` refuses
/// or an `out` too short for the bytes. After `None`, what `out` holds is
/// unspecified.
pub fn from_base64_into(text: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut used = 0usize;
    decode_base64(text, |bytes| {
        let end = used.checked_add(bytes.len())?;
        out.get_mut(used..end)?.copy_from_slice(bytes);
        used = end;
        Some(())
    })?;
    Some(used)
}

/// Every one to three bytes padded standard base64 `text` spells, in order,
/// or `None` at the first wrong spelling or the first `None` of `emit`.
fn decode_base64(text: &[u8], mut emit: impl FnMut(&[u8]) -> Option<()>) -> Option<()> {
    let (quads, rest) = text.as_chunks::<4>();
    if !rest.is_empty() {
        return None;
    }
    for (index, quad) in quads.iter().enumerate() {
        let pad = quad.iter().rev().take_while(|&&byte| byte == b'=').count();
        if pad > 2 || (pad > 0 && index + 1 != quads.len()) {
            return None;
        }
        let mut value = 0u32;
        for (at, &byte) in quad.iter().enumerate() {
            let sextet = if at < 4 - pad { sextet(byte)? } else { 0 };
            value = (value << 6) | u32::from(sextet);
        }
        // Padding stands for bytes not there; the bits it covers are zero.
        if value & ((1u32 << (8 * pad)) - 1) != 0 {
            return None;
        }
        emit(value.to_be_bytes().get(1..4 - pad)?)?;
    }
    Some(())
}

fn sextet(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// The alphabet symbol for a masked value; out of range would be a bug,
/// answered with `?` rather than a panic.
fn digit(alphabet: &[u8], value: u8) -> char {
    char::from(symbol(alphabet, value))
}

fn symbol(alphabet: &[u8], value: u8) -> u8 {
    alphabet.get(usize::from(value)).copied().unwrap_or(b'?')
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_every_byte_in_one_spelling() {
        let all: Vec<u8> = (0..=255).collect();
        let text = hex(&all);
        assert_eq!(text.len(), 512);
        assert!(text.starts_with("000102") && text.ends_with("fdfeff"));
        assert_eq!(from_hex(&text).unwrap(), all);
        assert_eq!(hex(b""), "");
        assert_eq!(from_hex(""), Some(Vec::new()));
    }

    #[test]
    fn hex_decoding_refuses_every_other_spelling() {
        for bad in [
            "0", "abc", "AB", "aB", " ab", "ab ", "+f", "-f", "0x", "gg", "é1",
        ] {
            assert_eq!(from_hex(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected);
        }
    }

    #[test]
    fn base64_covers_every_byte_and_a_basic_credential() {
        assert_eq!(
            base64(b"you@example.com:hunter2"),
            "eW91QGV4YW1wbGUuY29tOmh1bnRlcjI="
        );
        let all: Vec<u8> = (0..=255).collect();
        let encoded = base64(&all);
        assert_eq!(encoded.len(), 344);
        assert!(encoded.starts_with("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8g"));
        assert!(encoded.ends_with("+fr7/P3+/w=="));
    }

    #[test]
    fn base64_decodes_rfc_4648_vectors_and_every_byte() {
        for (expected, input) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(from_base64(input).unwrap(), expected.as_bytes(), "{input}");
        }
        for length in 0..=256 {
            let bytes: Vec<u8> = (0..length).map(|i| (i * 7 + 3) as u8).collect();
            assert_eq!(from_base64(&base64(&bytes)).unwrap(), bytes);
        }
    }

    #[test]
    fn base64_decoding_refuses_every_other_spelling() {
        for bad in [
            "Zg",
            "Zg=",
            "Zm9",
            "Z===",
            "====",
            "=Zg=",
            "Zg==Zg==",
            "Zm9v=",
            "Zh==",
            "Zm9=",
            "Zm 9v",
            " Zm9v",
            "Zm9v\n",
            "-_-_",
            "Zm9v\0\0\0\0",
            "Zé=",
            "Zg=a",
        ] {
            assert_eq!(from_base64(bad), None, "{bad:?}");
            assert_eq!(from_base64_into(bad.as_bytes(), &mut [0; 16]), None);
        }
    }

    #[test]
    fn slice_forms_agree_with_the_allocating_ones() {
        for length in 0..=64 {
            let bytes: Vec<u8> = (0..length).map(|i| (i * 13 + 5) as u8).collect();
            let text = base64(&bytes);
            let mut encoded = [0u8; 96];
            assert_eq!(base64_into(&bytes, &mut encoded), Some(text.len()));
            assert_eq!(&encoded[..text.len()], text.as_bytes());
            let mut decoded = [0u8; 64];
            assert_eq!(
                from_base64_into(text.as_bytes(), &mut decoded),
                Some(length)
            );
            assert_eq!(&decoded[..length], &bytes[..]);
        }
    }

    #[test]
    fn slice_forms_refuse_a_short_output() {
        let mut short = [7u8; 7];
        assert_eq!(base64_into(b"foobar", &mut short), None);
        assert_eq!(short, [7; 7], "a refused encoding writes nothing");
        let mut exact = [0u8; 8];
        assert_eq!(base64_into(b"foobar", &mut exact), Some(8));
        assert_eq!(&exact, b"Zm9vYmFy");
        assert_eq!(from_base64_into(b"Zm9vYmFy", &mut [0; 5]), None);
        assert_eq!(from_base64_into(b"Zm9vYg==", &mut [0; 3]), None);
        assert_eq!(from_base64_into(b"Zm9vYg==", &mut [0; 4]), Some(4));
        assert_eq!(from_base64_into(b"", &mut []), Some(0));
        assert_eq!(base64_into(b"", &mut []), Some(0));
    }
}
