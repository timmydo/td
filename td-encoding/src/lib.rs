//! Byte encodings td's crates each kept a copy of: lowercase hexadecimal
//! both ways, and base64 (RFC 4648 §4, the standard alphabet with `=`
//! padding) for encoding.
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
            out.push(if index > chunk.len() {
                '='
            } else {
                digit(BASE64, sextet)
            });
        }
    }
    out
}

/// The alphabet symbol for a masked value; out of range would be a bug,
/// answered with `?` rather than a panic.
fn digit(alphabet: &[u8], value: u8) -> char {
    alphabet
        .get(usize::from(value))
        .map_or('?', |byte| char::from(*byte))
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
}
