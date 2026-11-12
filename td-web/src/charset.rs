//! How a document's bytes become its text (DESIGN.md §5): the encoding
//! WHATWG's rules choose, through td-charset, and the bytes decoded to at
//! most a bound of UTF-8, malformed ones replaced, never refused.

use td_charset::sniff::{self, Source};
use td_charset::{Decoder, Encoding};

/// Which rules choose a document's encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rules {
    /// HTML's (HTML Standard, "Determining the character encoding"): a
    /// byte order mark, the `content-type`'s charset, a `<meta>` naming
    /// one in the first 1024 bytes, then UTF-8 when every byte shown is
    /// UTF-8 (the standard's optional detection step), else windows-1252,
    /// its default for a browser with no locale.
    Html,
    /// XML's, for `application/xhtml+xml` and XML shown as text: a byte
    /// order mark, the charset, else UTF-8. The XML declaration is not
    /// read.
    Xml,
    /// Plain text's: HTML's without the `<meta>`, as browsers read text
    /// (Firefox prescans no `text/plain`).
    Text,
}

/// The longest `charset` value kept: WHATWG's longest label,
/// `csisolatincyrillic`, is 18 bytes, so a longer value names no encoding
/// and is kept as an empty one, which names none either.
const MAX_LABEL: usize = 32;

/// A `charset` parameter's value, unquoted and unescaped.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Label {
    bytes: [u8; MAX_LABEL],
    len: usize,
}

impl Label {
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or_default()
    }
}

/// A document's bytes as text.
#[derive(Debug)]
pub struct Text {
    /// At most the bound's bytes of UTF-8, with no byte order mark.
    pub text: String,
    pub encoding: Encoding,
    /// How many of the body's bytes `text` holds, its byte order mark
    /// included: the body's length when it is all there.
    pub read: usize,
}

/// HTTP's whitespace.
fn http_space(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | b'\r' | b' ')
}

/// The first `charset` parameter of a `content-type` value, as WHATWG's
/// "parse a MIME type" reads its parameters: a `;` inside a quoted string
/// does not end it, `\` escapes there, no space may stand between a name
/// and its `=`, and a value that is empty unquoted, or holds a control
/// character, is no parameter.
pub fn label(content_type: &str) -> Option<Label> {
    let bytes = content_type.as_bytes();
    let mut at = bytes.iter().position(|&byte| byte == b';')?;
    // `at` is on the `;` before a parameter.
    loop {
        at += 1;
        while bytes.get(at).copied().is_some_and(http_space) {
            at += 1;
        }
        let name = at;
        while bytes
            .get(at)
            .is_some_and(|&byte| byte != b';' && byte != b'=')
        {
            at += 1;
        }
        let name = bytes.get(name..at).unwrap_or_default();
        match bytes.get(at) {
            None => return None,
            Some(b';') => continue,
            Some(_) => at += 1,
        }
        let mut value = Label::default();
        let mut long = false;
        let mut keep = |byte: u8| match value.bytes.get_mut(value.len) {
            Some(slot) => {
                *slot = byte;
                value.len += 1;
            }
            None => long = true,
        };
        let mut valid = true;
        if bytes.get(at) == Some(&b'"') {
            at += 1;
            loop {
                match bytes.get(at) {
                    None => break,
                    Some(b'"') => {
                        at += 1;
                        break;
                    }
                    Some(b'\\') => {
                        at += 1;
                        match bytes.get(at) {
                            None => {
                                keep(b'\\');
                                break;
                            }
                            Some(&byte) => {
                                valid &= quoted(byte);
                                keep(byte);
                                at += 1;
                            }
                        }
                    }
                    Some(&byte) => {
                        valid &= quoted(byte);
                        keep(byte);
                        at += 1;
                    }
                }
            }
            while bytes.get(at).is_some_and(|&byte| byte != b';') {
                at += 1;
            }
        } else {
            let start = at;
            while bytes.get(at).is_some_and(|&byte| byte != b';') {
                at += 1;
            }
            let mut raw = bytes.get(start..at).unwrap_or_default();
            while let Some((last, rest)) = raw.split_last() {
                if !http_space(*last) {
                    break;
                }
                raw = rest;
            }
            valid = !raw.is_empty() && raw.iter().all(|&byte| quoted(byte));
            raw.iter().for_each(|&byte| keep(byte));
        }
        if valid && name.eq_ignore_ascii_case(b"charset") {
            return Some(if long { Label::default() } else { value });
        }
        bytes.get(at)?;
    }
}

/// Whether `byte` may stand in a parameter's value: HTTP's quoted-string
/// token code points.
fn quoted(byte: u8) -> bool {
    matches!(byte, b'\t' | 0x20..=0x7e | 0x80..=0xff)
}

/// `body` as text by `rules`, `declared` the `content-type`'s charset:
/// its first `most` bytes of UTF-8, the scalars a byte completes left out
/// whole when the bound would cut them. A body already UTF-8 becomes the
/// text in place, its allocation kept; any other, and a UTF-8 body that
/// ends in an unfinished sequence, is decoded into a string of at most
/// `most` bytes and the body dropped. Fails only when that string cannot
/// be had.
pub fn decode(
    body: Vec<u8>,
    declared: Option<&[u8]>,
    rules: Rules,
    most: usize,
) -> Result<Text, td_alloc::Error> {
    let fallback = match rules {
        Rules::Xml => Encoding::Utf8,
        Rules::Html | Rules::Text => Encoding::Windows1252,
    };
    let chosen = sniff::sniff(&body, declared, rules == Rules::Html, fallback);
    let bom = chosen.bom_len;
    let utf8 = utf8(body.get(bom..).unwrap_or_default(), most);
    let encoding = match chosen.source {
        Source::Fallback if utf8 != Utf8::Not => Encoding::Utf8,
        _ => chosen.encoding,
    };
    let body = match utf8 {
        Utf8::InPlace(shown) if encoding == Encoding::Utf8 => {
            let mut bytes = body;
            bytes.truncate(bom.saturating_add(shown));
            match String::from_utf8(bytes) {
                // A UTF-8 byte order mark is a character of its own.
                Ok(mut text) if text.is_char_boundary(bom) => {
                    let read = text.len();
                    text.drain(..bom);
                    return Ok(Text {
                        text,
                        encoding,
                        read,
                    });
                }
                // Not reached: those bytes were found UTF-8.
                Ok(text) => text.into_bytes(),
                Err(not) => not.into_bytes(),
            }
        }
        _ => body,
    };
    decoded(body.get(bom..).unwrap_or_default(), encoding, most).map(|(text, read)| Text {
        text,
        encoding,
        read: bom.saturating_add(read),
    })
}

/// Whether bytes are UTF-8 within a bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Utf8 {
    /// They are, and this many of them are the text as they stand: all,
    /// or up to a character the bound cuts, which is left out.
    InPlace(usize),
    /// They are but for a sequence unfinished where the body ends, which
    /// decoding makes a replacement.
    Unfinished,
    /// A byte shown is not UTF-8.
    Not,
}

/// Whether `bytes` are UTF-8 as far as `most` shows them. A sequence
/// the bound cuts counts when the bytes past it finish it. One the body
/// ends in counts only after other UTF-8 beyond ASCII: a lone lead byte
/// at the end of ASCII is as likely the last letter of windows-1252, as
/// `caf\xe9` is, as a sequence a server cut short.
fn utf8(bytes: &[u8], most: usize) -> Utf8 {
    let shown = bytes.get(..most).unwrap_or(bytes);
    let cut = match std::str::from_utf8(shown) {
        Ok(_) => return Utf8::InPlace(shown.len()),
        Err(cut) if cut.error_len().is_none() => cut.valid_up_to(),
        Err(_) => return Utf8::Not,
    };
    let rest = bytes.get(cut..).unwrap_or_default();
    let next = rest.get(..4).unwrap_or(rest);
    match std::str::from_utf8(next) {
        // The bound cut a character the bytes past it finish.
        Ok(_) => return Utf8::InPlace(cut),
        Err(after) if after.valid_up_to() > 0 => return Utf8::InPlace(cut),
        Err(after) if after.error_len().is_some() => return Utf8::Not,
        Err(_) => {}
    }
    // The body ends in the sequence.
    let wide = shown.get(..cut).is_some_and(|valid| !valid.is_ascii());
    match (wide, bytes.len() > most) {
        (false, _) => Utf8::Not,
        (true, true) => Utf8::InPlace(cut),
        (true, false) => Utf8::Unfinished,
    }
}

/// `bytes` decoded from `encoding` to at most `most` bytes of UTF-8, and
/// how many of the bytes that holds: those before the first batch the
/// bound refused, less any the decoder still held unfinished. The string
/// is had once: a byte decodes to at most three bytes of UTF-8 (a scalar
/// of four takes four), the end to one replacement more.
fn decoded(
    bytes: &[u8],
    encoding: Encoding,
    most: usize,
) -> Result<(String, usize), td_alloc::Error> {
    let mut text = String::new();
    let room = bytes.len().saturating_add(1).saturating_mul(3).min(most);
    td_alloc::reserve_string(&mut text, room)?;
    let mut decoder = Decoder::new(encoding);
    let mut read = 0;
    for (at, &byte) in bytes.iter().enumerate() {
        // Never finished before the end. Called by path: the allocation
        // guard counts every `.push(` as growth, and this push allocates
        // nothing, so a debt row for it would record no allocation.
        let Ok(scalars) = Decoder::push(&mut decoder, byte) else {
            break;
        };
        if !fits(&text, &scalars, most) {
            return Ok((text, read));
        }
        append(&mut text, &scalars)?;
        read = (at + 1).saturating_sub(decoder.pending());
    }
    let last = decoder.finish();
    if fits(&text, &last, most) {
        append(&mut text, &last)?;
        read = bytes.len();
    }
    Ok((text, read))
}

fn fits(text: &str, scalars: &td_charset::Decoded, most: usize) -> bool {
    let more: usize = scalars.chars().map(char::len_utf8).sum();
    text.len().saturating_add(more) <= most
}

fn append(text: &mut String, scalars: &td_charset::Decoded) -> Result<(), td_alloc::Error> {
    let mut utf8 = [0; 4];
    for scalar in scalars.chars() {
        td_alloc::push_str(text, scalar.encode_utf8(&mut utf8))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn text(body: &[u8], declared: Option<&str>, rules: Rules) -> (String, &'static str) {
        let declared = declared.map(str::as_bytes);
        let decoded = decode(body.to_vec(), declared, rules, 1 << 20).unwrap();
        assert_eq!(decoded.read, body.len());
        (decoded.text, decoded.encoding.name())
    }

    fn charset(content_type: &str) -> Option<String> {
        label(content_type).map(|label| String::from_utf8(label.as_bytes().to_vec()).unwrap())
    }

    /// WHATWG's MIME type parameters: the first `charset` that is one,
    /// its quoted string unescaped and a `;` inside it no end.
    #[test]
    fn the_charset_is_the_content_types_first_parameter_of_that_name() {
        let some = |label: &str| Some(label.to_string());
        assert_eq!(charset("text/html"), None);
        assert_eq!(charset("text/html; charset=utf-8"), some("utf-8"));
        assert_eq!(
            charset("text/html;CHARSET=\"Shift_JIS\" "),
            some("Shift_JIS")
        );
        assert_eq!(charset("text/html; charset =x"), None, "no space before =");
        assert_eq!(
            charset("text/html; charset =x; charset=utf-8"),
            some("utf-8")
        );
        assert_eq!(
            charset("text/plain; format=flowed; charset=iso-8859-2; charset=utf-8"),
            some("iso-8859-2")
        );
        assert_eq!(
            charset("text/html; foo=\"a;charset=x\"; charset=windows-1252"),
            some("windows-1252")
        );
        assert_eq!(charset("text/html; charset=\"utf\\-8\""), some("utf-8"));
        assert_eq!(charset("text/plain; charset"), None);
        assert_eq!(
            charset("text/plain; charset=; charset=koi8-r"),
            some("koi8-r")
        );
        assert_eq!(charset("text/plain; charset=\"unclosed"), some("unclosed"));
        assert_eq!(charset("text/plain; charset=utf-8  "), some("utf-8"));
        let long = format!("text/plain; charset={}", "x".repeat(MAX_LABEL + 1));
        assert_eq!(charset(&long), some(""), "too long to name an encoding");
    }

    /// A byte order mark, then the label, then (HTML's alone) a `<meta>`,
    /// then UTF-8 when every byte is, else windows-1252 (XML's UTF-8).
    #[test]
    fn the_encoding_is_chosen_in_whatwg_s_order() {
        let html = Rules::Html;
        assert_eq!(
            text(b"\xef\xbb\xbfcaf\xc3\xa9", Some("windows-1252"), html),
            ("café".into(), "UTF-8")
        );
        assert_eq!(
            text(b"caf\xe9", Some("latin1"), html),
            ("café".into(), "windows-1252")
        );
        assert_eq!(
            text(b"<meta charset=iso-8859-2>\xb1", Some("bogus"), html),
            ("<meta charset=iso-8859-2>ą".into(), "ISO-8859-2")
        );
        assert_eq!(
            text(b"<meta charset=iso-8859-2>\xb1", None, Rules::Text),
            ("<meta charset=iso-8859-2>±".into(), "windows-1252")
        );
        assert_eq!(text(b"caf\xc3\xa9", None, html), ("café".into(), "UTF-8"));
        assert_eq!(
            text(b"caf\xe9", None, html),
            ("café".into(), "windows-1252")
        );
        // All or nothing: one byte that is not UTF-8 makes the whole page
        // windows-1252.
        assert_eq!(
            text(b"caf\xe9 na\xc3\xafve", None, Rules::Text),
            ("café naÃ¯ve".into(), "windows-1252")
        );
        assert_eq!(
            text(b"caf\xe9", None, Rules::Xml),
            ("caf\u{fffd}".into(), "UTF-8")
        );
        assert_eq!(
            text(b"\xff\xfe\xe9\x00\xac\x20", Some("utf-8"), html),
            ("é€".into(), "UTF-16LE")
        );
    }

    /// A UTF-8 body with no label that ends in an unfinished sequence is
    /// still UTF-8, the sequence one replacement.
    #[test]
    fn utf8_ending_unfinished_is_still_utf8() {
        let body = b"na\xc3\xafve caf\xc3";
        assert_eq!(
            text(body, None, Rules::Text),
            ("naïve caf\u{fffd}".into(), "UTF-8")
        );
        assert_eq!(
            text(body, None, Rules::Html),
            ("naïve caf\u{fffd}".into(), "UTF-8")
        );
        // After ASCII alone a lead byte at the end is windows-1252's.
        assert_eq!(
            text(b"caf\xe9", None, Rules::Text),
            ("café".into(), "windows-1252")
        );
        // The bound cutting a character the bytes past it finish, or that
        // they show is not UTF-8.
        let decoded = decode(b"caf\xc3\xa9!".to_vec(), None, Rules::Text, 4).unwrap();
        assert_eq!(
            (decoded.text.as_str(), decoded.encoding),
            ("caf", Encoding::Utf8)
        );
        let decoded = decode(b"caf\xc3!".to_vec(), None, Rules::Text, 4).unwrap();
        assert_eq!(decoded.encoding, Encoding::Windows1252);
        // Within a bound the replacement does not fit: the sequence is
        // not shown and not counted.
        let decoded = decode(body.to_vec(), None, Rules::Text, 11).unwrap();
        assert_eq!((decoded.text.as_str(), decoded.read), ("naïve caf", 10));
    }

    /// Malformed bytes are each a replacement, as is a sequence the
    /// body ends in, and the text goes on after them.
    #[test]
    fn malformed_bytes_are_replaced() {
        assert_eq!(
            text(b"a\xffb\xc3", Some("utf-8"), Rules::Html).0,
            "a\u{fffd}b\u{fffd}"
        );
        assert_eq!(
            text(b"<p>\x93\xfa\xff<\x93", Some("shift_jis"), Rules::Html).0,
            "<p>日\u{fffd}<\u{fffd}"
        );
    }

    /// The bound is on the text's UTF-8, here three bytes a byte, which
    /// is all the string ever holds; a UTF-8 body is cut where a
    /// character starts.
    #[test]
    fn the_text_is_bounded_in_utf8_however_the_bytes_expand() {
        let most = 3000;
        for (byte, label) in [(0x80, "windows-1252"), (0xb1, "shift_jis"), (0xff, "utf-8")] {
            let decoded = decode(vec![byte; most], Some(label.as_bytes()), Rules::Text, most);
            let decoded = decoded.unwrap();
            assert_eq!(decoded.text.len(), most, "{label}");
            assert_eq!(decoded.read, most / 3, "{label}");
            assert!(decoded.text.capacity() <= most, "{label}");
        }
        let decoded = decode(vec![0x80; 10], Some(b"windows-1252"), Rules::Text, most).unwrap();
        assert_eq!((decoded.text.len(), decoded.read), (30, 10));
        let decoded = decode("é".repeat(most).into_bytes(), None, Rules::Text, most + 1).unwrap();
        assert_eq!((decoded.text.len(), decoded.read), (most, most));
        assert_eq!(
            decoded.encoding,
            Encoding::Utf8,
            "the cut character is no error"
        );
        let mut marked = b"\xef\xbb\xbf".to_vec();
        marked.extend(b"x".repeat(most));
        let decoded = decode(marked, None, Rules::Text, 10).unwrap();
        assert_eq!((decoded.text.as_str(), decoded.read), ("xxxxxxxxxx", 13));
    }

    /// What is read is where the first batch the bound refused began,
    /// less what the decoder still held unfinished: never a byte whose
    /// scalars are not shown.
    #[test]
    fn read_counts_only_the_bytes_shown() {
        for (label, body, most, shown, read) in [
            (
                "shift_jis",
                b"\x93\xfa\x93\xfa\x93\xfa".as_slice(),
                4,
                "日",
                2,
            ),
            ("gb18030", b"ab\x81\x30\x81", 2, "ab", 2),
            ("utf-16le", b"a\x00\x3d\xd8\x00", 1, "a", 2),
            // A replayed batch of three scalars refused whole.
            ("gb18030", b"\x81\x30\x81\x78", 5, "", 0),
            ("shift_jis", b"a\x93\xfa", 1, "a", 1),
            ("utf-16le", b"\xff\xfea\x00", 0, "", 2),
            ("utf-8", b"\xc3\xc3\xa9", 3, "\u{fffd}", 1),
            ("replacement", b"abcd", 3, "\u{fffd}", 4),
        ] {
            let decoded = decode(body.to_vec(), Some(label.as_bytes()), Rules::Text, most);
            let decoded = decoded.unwrap();
            assert_eq!(
                (decoded.text.as_str(), decoded.read),
                (shown, read),
                "{label} {body:x?}"
            );
        }
    }
}
