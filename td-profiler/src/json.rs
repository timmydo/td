use std::fmt::Write as _;

pub fn string(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut out = String::with_capacity(text.len().saturating_add(2));
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c <= '\u{1f}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    Some(out)
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        out.push(char::from(
            *DIGITS.get(usize::from(byte >> 4)).unwrap_or(&b'0'),
        ));
        out.push(char::from(
            *DIGITS.get(usize::from(byte & 0x0f)).unwrap_or(&b'0'),
        ));
    }
    out
}

pub fn named_bytes(name: &str, bytes: &[u8]) -> String {
    let mut out = String::new();
    if let Some(value) = string(bytes) {
        let _ = write!(out, "\"{name}\":{value},");
    }
    let _ = write!(out, "\"{name}_bytes\":\"{}\"", hex(bytes));
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{named_bytes, string};

    #[test]
    fn invalid_utf8_has_only_hex_identity() {
        assert_eq!(named_bytes("path", &[0xff, 0]), "\"path_bytes\":\"ff00\"");
        assert_eq!(string(b"a\n\"b"), Some("\"a\\n\\\"b\"".into()));
    }
}
