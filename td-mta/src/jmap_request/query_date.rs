//! Strict UTC whole-second query bounds used by td-mail.
use super::Error;
pub(super) fn parse(bytes: &[u8]) -> Result<i64, Error> {
    if bytes.len() != 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
        || bytes.get(19) != Some(&b'Z')
    {
        return Err(Error::InvalidArguments);
    }
    for (index, byte) in bytes.iter().enumerate() {
        if !matches!(index, 4 | 7 | 10 | 13 | 16 | 19) && !byte.is_ascii_digit() {
            return Err(Error::InvalidArguments);
        }
    }
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.get(..19))
        .and_then(td_civil::parse_ymd_hms)
        .and_then(|civil| td_civil::civil_to_unix_utc(&civil))
        .and_then(|seconds| seconds.checked_mul(1000))
        .ok_or(Error::InvalidArguments)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dates_are_exact_utc_seconds_and_do_not_clamp() {
        assert_eq!(parse(b"1970-01-01T00:00:00Z"), Ok(0));
        assert_eq!(parse(b"1969-12-31T23:59:59Z"), Ok(-1000));
        for value in [
            "2024-01-01T00:00:0 Z",
            "2024-01-01T00:00: 0Z",
            "2025-02-29T00:00:00Z",
            "2024-01-01T00:00:60Z",
            "2024-01-01T00:00:00+00:00",
            "2024-01-01T00:00:00.1Z",
        ] {
            assert_eq!(parse(value.as_bytes()), Err(Error::InvalidArguments));
        }
    }
}
