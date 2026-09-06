//! Base64 encoding (RFC 4648 §4, the standard alphabet with `=` padding), the
//! one direction td-mail needs: an HTTP Basic credential is `base64(user:pass)`.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// The standard encoding of `input`, padded.
pub fn encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let (whole, remainder) = input.as_chunks::<3>();
    for [a, b, c] in whole {
        let word = (u32::from(*a) << 16) | (u32::from(*b) << 8) | u32::from(*c);
        for shift in [18u32, 12, 6, 0] {
            out.push(symbol((word >> shift) & 0x3f));
        }
    }
    match remainder {
        [a] => {
            let word = u32::from(*a) << 16;
            out.push(symbol((word >> 18) & 0x3f));
            out.push(symbol((word >> 12) & 0x3f));
            out.push_str("==");
        }
        [a, b] => {
            let word = (u32::from(*a) << 16) | (u32::from(*b) << 8);
            out.push(symbol((word >> 18) & 0x3f));
            out.push(symbol((word >> 12) & 0x3f));
            out.push(symbol((word >> 6) & 0x3f));
            out.push('=');
        }
        _ => {}
    }
    out
}

/// The alphabet symbol for a six-bit value; the mask above keeps it in range,
/// and an out-of-range value would be a bug, answered with `=` rather than a
/// panic.
fn symbol(six: u32) -> char {
    ALPHABET
        .get(six as usize)
        .map_or('=', |byte| char::from(*byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_4648_vectors() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn basic_credential_and_every_byte() {
        assert_eq!(
            encode(b"you@example.com:hunter2"),
            "eW91QGV4YW1wbGUuY29tOmh1bnRlcjI="
        );
        let all: Vec<u8> = (0..=255u8).collect();
        let encoded = encode(&all);
        assert_eq!(encoded.len(), 344);
        assert!(encoded.starts_with("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8g"));
        assert!(encoded.ends_with("+fr7/P3+/w=="));
    }
}
