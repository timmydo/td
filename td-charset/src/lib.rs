//! WHATWG labels, allocation-free incremental decoding and bounded charset sniffing.
//! See DESIGN.md for the supported subset and policy boundaries.
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod decoder;
mod labels;
pub mod sniff;
pub use decoder::{Decoded, Decoder, Finished};
pub use labels::Encoding;

impl Encoding {
    /// Resolve a WHATWG label, trimming only ASCII whitespace. Unsupported labels
    /// return None; notably ISO-8859-1/ASCII mean windows-1252, not strict Latin-1.
    pub fn for_label(label: &[u8]) -> Option<Self> {
        let label = trim(label);
        labels::LABELS
            .iter()
            .find_map(|(name, encoding)| label.eq_ignore_ascii_case(name).then_some(*encoding))
    }
}

pub(crate) fn space(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | 0x0c | b'\r' | b' ')
}
pub(crate) fn trim(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(|b| space(*b)) {
        bytes = bytes.get(1..).unwrap_or_default();
    }
    while bytes.last().is_some_and(|b| space(*b)) {
        bytes = bytes.get(..bytes.len() - 1).unwrap_or_default();
    }
    bytes
}

/// A complete bounded decoding result. Error replacements are distinguished from
/// literal U+FFFD characters in the input.
#[derive(Debug, Eq, PartialEq)]
pub struct Text {
    pub text: String,
    pub had_errors: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputLimit;
impl std::fmt::Display for OutputLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("decoded UTF-8 output limit or allocation failure")
    }
}
impl std::error::Error for OutputLimit {}

/// Decode with replacement and a caller-supplied UTF-8 byte ceiling. This raw
/// decoder preserves BOMs. To strip a detected BOM use sniff::Selection::body.
/// On refusal no partial output is returned. Streaming consumers can avoid all
/// allocations with Decoder instead.
pub fn decode(encoding: Encoding, bytes: &[u8], max_output: usize) -> Result<Text, OutputLimit> {
    let mut decoder = Decoder::new(encoding);
    let mut result = Text {
        text: String::new(),
        had_errors: false,
    };
    for chunk in bytes.iter().map(|byte| decoder.push(*byte)) {
        // No finish occurs inside this loop, so push cannot be closed.
        append(&mut result, chunk.map_err(|_| OutputLimit)?, max_output)?;
    }
    append(&mut result, decoder.finish(), max_output)?;
    Ok(result)
}
fn append(out: &mut Text, decoded: Decoded, limit: usize) -> Result<(), OutputLimit> {
    for ch in decoded.chars() {
        let length = out
            .text
            .len()
            .checked_add(ch.len_utf8())
            .ok_or(OutputLimit)?;
        if length > limit {
            return Err(OutputLimit);
        }
        out.text
            .try_reserve(ch.len_utf8())
            .map_err(|_| OutputLimit)?;
        out.text.push(ch);
    }
    out.had_errors |= decoded.had_errors;
    Ok(())
}

#[cfg(test)]
mod tests;
