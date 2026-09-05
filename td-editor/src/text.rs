//! Byte-preserving file codec and the shared scalar/column conventions.

use crate::{Error, Result};

pub const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
pub const TAB_WIDTH: usize = 8;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LineEnding {
    #[default]
    Lf,
    CrLf,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Format {
    pub bom: bool,
    pub ending: LineEnding,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Decoded {
    pub text: String,
    pub format: Format,
}

pub fn validate(text: &str) -> Result<()> {
    if text.starts_with('\u{feff}')
        || text
            .chars()
            .any(|c| (c <= '\u{1f}' && c != '\n' && c != '\t') || c == '\u{7f}')
    {
        return Err(Error::InvalidText);
    }
    Ok(())
}

/// Insertion fragments may contain interior BOM scalars. The transaction
/// checks the resulting document's first scalar, even after a deletion.
pub fn insertion(text: &str) -> Result<String> {
    if text.len() > MAX_FILE_BYTES {
        return Err(Error::Limit);
    }
    let normalized = text.replace("\r\n", "\n");
    if normalized
        .chars()
        .any(|c| (c <= '\u{1f}' && c != '\n' && c != '\t') || c == '\u{7f}')
    {
        return Err(Error::InvalidText);
    }
    Ok(normalized)
}

pub fn decode(bytes: &[u8]) -> Result<Decoded> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(Error::Limit);
    }
    let input = std::str::from_utf8(bytes).map_err(|_| Error::InvalidText)?;
    let bom = input.starts_with('\u{feff}');
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let crlf = input.matches("\r\n").count();
    let newlines = input.bytes().filter(|&b| b == b'\n').count();
    if crlf != 0 && crlf != newlines {
        return Err(Error::InvalidText);
    }
    let text = insertion(input)?;
    validate(&text)?;
    Ok(Decoded {
        text,
        format: Format {
            bom,
            ending: if crlf == 0 {
                LineEnding::Lf
            } else {
                LineEnding::CrLf
            },
        },
    })
}

pub fn encoded_len(bytes: usize, newlines: usize, format: Format) -> Result<usize> {
    bytes
        .checked_add(if format.bom { 3 } else { 0 })
        .and_then(|n| {
            n.checked_add(if format.ending == LineEnding::CrLf {
                newlines
            } else {
                0
            })
        })
        .ok_or(Error::Limit)
}

pub fn encode(text: &str, format: Format) -> Result<Vec<u8>> {
    validate(text)?;
    let size = encoded_len(
        text.len(),
        text.bytes().filter(|&b| b == b'\n').count(),
        format,
    )?;
    if size > MAX_FILE_BYTES {
        return Err(Error::Limit);
    }
    let mut bytes = Vec::with_capacity(size);
    if format.bom {
        bytes.extend_from_slice(b"\xef\xbb\xbf");
    }
    for byte in text.bytes() {
        if byte == b'\n' && format.ending == LineEnding::CrLf {
            bytes.push(b'\r');
        }
        bytes.push(byte);
    }
    Ok(bytes)
}

pub fn column(text: &str) -> usize {
    text.chars().fold(0usize, |col, c| match c {
        '\n' => 0,
        '\t' => col.saturating_add(TAB_WIDTH - col % TAB_WIDTH),
        _ => col.saturating_add(1),
    })
}

pub fn line(text: &str, caret: usize) -> Result<std::ops::Range<usize>> {
    let before = text.get(..caret).ok_or(Error::InvalidPosition)?;
    let after = text.get(caret..).ok_or(Error::InvalidPosition)?;
    let start = before.rfind('\n').map_or(0, |n| n + 1);
    let end = after.find('\n').map_or(text.len(), |n| caret + n);
    Ok(start..end)
}
