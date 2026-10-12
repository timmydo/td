//! Bounded JMAP UTCDate output for stored Unix milliseconds.
use crate::bounded::TextBuffer;

/// Refusal to format a stored timestamp; no output prefix is published.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The timestamp has no four-digit RFC 3339 year.
    OutOfRange,
    /// The caller-owned output refused the complete formatted value.
    Output(crate::bounded::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfRange => f.write_str("timestamp outside JMAP UTCDate range"),
            Self::Output(error) => write!(f, "JMAP UTCDate output: {error}"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::OutOfRange => None,
            Self::Output(error) => Some(error),
        }
    }
}

/// Append one unquoted UTCDate atomically. The caller budgets the fixed work
/// and at most 24 output bytes; this formatter grants no publication authority.
pub fn write_utc_ms(utc_ms: i64, output: &mut TextBuffer<'_>) -> Result<(), Error> {
    let civil = td_civil::unix_to_civil_utc_checked(utc_ms.div_euclid(1000))
        .filter(|civil| (0..=9999).contains(&civil.year))
        .ok_or(Error::OutOfRange)?;
    let fraction = utc_ms.rem_euclid(1000);
    let result = if fraction == 0 {
        output.format(format_args!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            civil.year, civil.month, civil.day, civil.hour, civil.minute, civil.second
        ))
    } else {
        output.format(format_args!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            civil.year, civil.month, civil.day, civil.hour, civil.minute, civil.second, fraction
        ))
    };
    result.map_err(Error::Output)
}

#[cfg(test)]
#[path = "jmap_date_tests.rs"]
mod tests;
