//! Fixed-state RFC header lexical cursors with caller-owned admission.
#![forbid(unsafe_code)]
pub mod cfws;
pub mod delimited;
pub mod language_tag;
pub mod mime_attribute;
pub mod mime_boundary;
pub mod mime_protocol;
pub mod mime_value;
pub mod projection;
pub mod resident;
#[derive(Clone, Copy, Default, Debug, Eq, PartialEq)]
/// Logical source visits and transitions, including zero-byte EOF attempts.
pub struct Charge {
    pub visits: u64,
    pub records: u64,
}
/// Admit before access; bind current clock/cancellation and caller budgets.
/// The callback must be bounded and must admit even zero-count calls.
pub trait Work {
    type Error: Copy;
    fn charge(&mut self, charge: Charge) -> Result<(), Self::Error>;
}
/// One ASCII MIME token octet; placement and complete-token validity are external.
#[inline]
#[must_use]
pub const fn mime_token_octet(byte: u8) -> bool {
    match byte {
        b'(' | b')' | b'<' | b'>' | b'@' | b',' | b';' | b':' | b'\\' | b'"' | b'/' | b'['
        | b']' | b'?' | b'=' => false,
        33..=126 => true,
        _ => false,
    }
}
#[cfg(test)]
mod tests;
