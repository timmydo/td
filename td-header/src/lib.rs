//! Fixed-state RFC header lexical cursors with caller-owned admission.
#![forbid(unsafe_code)]
pub mod cfws;
pub mod delimited;
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
#[cfg(test)]
mod tests;
