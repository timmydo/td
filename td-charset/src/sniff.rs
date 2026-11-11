//! Deterministic BOM / declared-label / HTML-meta / fallback selection.
//! This is a bounded metadata sniffer, not statistical language detection or
//! the browser's complete document-encoding algorithm (DESIGN.md).
use crate::{space, Encoding};

pub const HTML_SCAN_LIMIT: usize = 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    Bom,
    Declared,
    HtmlMeta,
    Fallback,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    pub encoding: Encoding,
    pub source: Source,
    /// Only a recognized BOM contributes bytes to skip.
    pub bom_len: usize,
}
impl Selection {
    /// Apply to the same complete byte stream passed to sniff.
    pub fn body(self, bytes: &[u8]) -> Option<&[u8]> {
        bytes.get(self.bom_len..)
    }
}
/// Call after collecting min(1024, complete input length) bytes for HTML, or at
/// least three bytes / EOF for other input. An incomplete prefix can otherwise
/// miss a BOM or a later meta declaration. `declared` is an already extracted
/// transport/MIME charset label, not a whole Content-Type header.
/// BOM wins over a supported declaration; unsupported labels are ignored.
/// HTML scanning is opt-in so arbitrary mail/news text cannot act as metadata.
pub fn sniff(bytes: &[u8], declared: Option<&[u8]>, html: bool, fallback: Encoding) -> Selection {
    for (mark, encoding) in [
        (b"\xef\xbb\xbf".as_slice(), Encoding::Utf8),
        (b"\xff\xfe".as_slice(), Encoding::Utf16Le),
        (b"\xfe\xff".as_slice(), Encoding::Utf16Be),
    ] {
        if bytes.starts_with(mark) {
            return Selection {
                encoding,
                source: Source::Bom,
                bom_len: mark.len(),
            };
        }
    }
    if let Some(encoding) = declared.and_then(Encoding::for_label) {
        return Selection {
            encoding,
            source: Source::Declared,
            bom_len: 0,
        };
    }
    if html {
        if let Some(encoding) = html_meta(bytes) {
            return Selection {
                encoding,
                source: Source::HtmlMeta,
                bom_len: 0,
            };
        }
    }
    Selection {
        encoding: fallback,
        source: Source::Fallback,
        bom_len: 0,
    }
}

/// Prescan complete meta tags in the first 1024 bytes. Follows HTML's byte
/// prescan rules for comments, tag skipping, attributes, duplicates and pragmas.
/// A token cut by the byte limit is ignored. Does not tokenize the document.
pub fn html_meta(input: &[u8]) -> Option<Encoding> {
    let bytes = input.get(..input.len().min(HTML_SCAN_LIMIT))?;
    let mut p = 0;
    while p < bytes.len() {
        let tail = bytes.get(p..)?;
        if tail.starts_with(b"<!--") {
            p += 2;
            while !bytes.get(p..)?.starts_with(b"-->") {
                p += 1;
            }
            p += 3;
            continue;
        }
        if tail
            .get(..5)
            .is_some_and(|s| s.eq_ignore_ascii_case(b"<meta"))
            && tail.get(5).is_some_and(|b| space(*b) || *b == b'/')
        {
            p += 5;
            let mut seen = 0u8;
            let mut pragma = false;
            let mut needs_pragma = None;
            let mut encoding = None;
            while let Some((name, value)) = attribute(bytes, &mut p) {
                let bit = if name.eq_ignore_ascii_case(b"http-equiv") {
                    1
                } else if name.eq_ignore_ascii_case(b"content") {
                    2
                } else if name.eq_ignore_ascii_case(b"charset") {
                    4
                } else {
                    0
                };
                if bit == 0 || seen & bit != 0 {
                    continue;
                }
                seen |= bit;
                match bit {
                    1 => pragma = value.eq_ignore_ascii_case(b"content-type"),
                    2 if encoding.is_none() => {
                        if let Some(found) = meta_content(value) {
                            encoding = Some(Some(found));
                            needs_pragma = Some(true);
                        }
                    }
                    4 => {
                        encoding = Some(Encoding::for_label(value));
                        needs_pragma = Some(false);
                    }
                    _ => {}
                }
            }
            // EOF also terminates an attribute; it never completes the tag.
            if bytes.get(p) != Some(&b'>') {
                return None;
            }
            if needs_pragma == Some(false) || (needs_pragma == Some(true) && pragma) {
                if let Some(Some(encoding)) = encoding {
                    return Some(match encoding {
                        Encoding::Utf16Le | Encoding::Utf16Be => Encoding::Utf8,
                        Encoding::UserDefined => Encoding::Windows1252,
                        other => other,
                    });
                }
            }
        } else if tail.first() == Some(&b'<') {
            let offset = if tail.get(1) == Some(&b'/') { 2 } else { 1 };
            if tail.get(offset).is_some_and(u8::is_ascii_alphabetic) {
                p += offset;
                while bytes.get(p).is_some_and(|b| !space(*b) && *b != b'>') {
                    p += 1;
                }
                while attribute(bytes, &mut p).is_some() {}
            } else if tail.get(1).is_some_and(|b| matches!(b, b'!' | b'/' | b'?')) {
                while bytes.get(p).is_some_and(|b| *b != b'>') {
                    p += 1;
                }
            }
        }
        p += 1;
    }
    None
}
fn attribute<'a>(bytes: &'a [u8], p: &mut usize) -> Option<(&'a [u8], &'a [u8])> {
    while bytes.get(*p).is_some_and(|b| space(*b) || *b == b'/') {
        *p += 1;
    }
    if *bytes.get(*p)? == b'>' {
        return None;
    }
    let start = *p;
    loop {
        let b = *bytes.get(*p)?;
        if space(b) || matches!(b, b'/' | b'>') || (b == b'=' && *p > start) {
            break;
        }
        *p += 1;
    }
    let name = bytes.get(start..*p)?;
    while bytes.get(*p).is_some_and(|b| space(*b)) {
        *p += 1;
    }
    if *bytes.get(*p)? != b'=' {
        return Some((name, b""));
    }
    *p += 1;
    while bytes.get(*p).is_some_and(|b| space(*b)) {
        *p += 1;
    }
    let first = *bytes.get(*p)?;
    if matches!(first, b'\'' | b'"') {
        *p += 1;
        let start = *p;
        while *bytes.get(*p)? != first {
            *p += 1;
        }
        let value = bytes.get(start..*p)?;
        *p += 1;
        Some((name, value))
    } else {
        let start = *p;
        while bytes.get(*p).is_some_and(|b| !space(*b) && *b != b'>') {
            *p += 1;
        }
        Some((name, bytes.get(start..*p)?))
    }
}
// HTML's extraction algorithm, deliberately distinct from MIME parameter syntax.
fn meta_content(bytes: &[u8]) -> Option<Encoding> {
    let mut p = 0;
    while p < bytes.len() {
        if !bytes
            .get(p..p + 7)
            .is_some_and(|s| s.eq_ignore_ascii_case(b"charset"))
        {
            p += 1;
            continue;
        }
        p += 7;
        while bytes.get(p).is_some_and(|b| space(*b)) {
            p += 1;
        }
        if bytes.get(p) != Some(&b'=') {
            continue;
        }
        p += 1;
        while bytes.get(p).is_some_and(|b| space(*b)) {
            p += 1;
        }
        let first = *bytes.get(p)?;
        if matches!(first, b'\'' | b'"') {
            p += 1;
            let start = p;
            while *bytes.get(p)? != first {
                p += 1;
            }
            return Encoding::for_label(bytes.get(start..p)?);
        }
        let start = p;
        while bytes.get(p).is_some_and(|b| !space(*b) && *b != b';') {
            p += 1;
        }
        return Encoding::for_label(bytes.get(start..p)?);
    }
    None
}
