//! Cold, offline generation from the approved IANA leap-second source.
#![forbid(unsafe_code)]
use std::{
    fmt::Write as _,
    fs::File,
    io::{self, Read},
    path::Path,
};
#[path = "../../engine/src/sha256.rs"]
#[allow(dead_code)]
mod sha256;
const BYTES: usize = 5065;
const HASH: &str = "db5a895f16853b03bfc865e8d68f9fc8710ef1740e3400c701cd46a5bbbc3433";
fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}
fn read(directory: &Path) -> io::Result<String> {
    let path = directory.join("leap-seconds.list");
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.len() != BYTES as u64 {
        return Err(invalid(
            "leap source must be a regular file of pinned length",
        ));
    }
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != BYTES as u64 {
        return Err(invalid("opened leap source differs in type or length"));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(BYTES + 1)
        .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
    file.take((BYTES + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() != BYTES {
        return Err(invalid("leap source length changed"));
    }
    let mut digest = sha256::Sha256::new();
    digest.update(&bytes);
    let mut hex = String::new();
    for byte in digest.finalize() {
        write!(hex, "{byte:02x}").map_err(io::Error::other)?;
    }
    if hex != HASH {
        return Err(invalid("leap source SHA-256 differs from approved pin"));
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}
fn number(text: &str) -> io::Result<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid("expected unsigned decimal leap source number"));
    }
    text.parse()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}
fn month_days(year: u32, month: u32) -> u64 {
    match month {
        2 if year.is_multiple_of(400) || (year.is_multiple_of(4) && !year.is_multiple_of(100)) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}
// Cold conversion, bounded to the admitted Gregorian year range.
fn calendar(mut days: u64) -> io::Result<(u32, u32, u32)> {
    for year in 1900..=9999 {
        let length = if month_days(year, 2) == 29 { 366 } else { 365 };
        if days >= length {
            days -= length;
            continue;
        }
        for month in 1..=12 {
            let length = month_days(year, month);
            if days >= length {
                days -= length;
                continue;
            }
            let day = u32::try_from(days + 1).map_err(io::Error::other)?;
            return Ok((year, month, day));
        }
        return Err(invalid("invalid leap calendar state"));
    }
    Err(invalid("leap timestamp exceeds four-digit year range"))
}
#[derive(Debug)]
struct Table {
    dates: Vec<u32>,
    updated: u64,
    expires: u64,
}
fn parse(text: &str) -> io::Result<Table> {
    let mut updated = None;
    let mut expires = None;
    let mut previous: Option<(u64, u64)> = None;
    let mut dates = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("#$") {
            if updated.replace(number(value.trim())?).is_some() {
                return Err(invalid("duplicate leap update timestamp"));
            }
        } else if let Some(value) = line.strip_prefix("#@") {
            if expires.replace(number(value.trim())?).is_some() {
                return Err(invalid("duplicate leap expiration timestamp"));
            }
        } else if !line.is_empty() && !line.starts_with('#') {
            let content = line.split_once('#').map_or(line, |(data, _)| data);
            let mut fields = content.split_ascii_whitespace();
            let ntp = number(
                fields
                    .next()
                    .ok_or_else(|| invalid("missing leap timestamp"))?,
            )?;
            let offset = number(
                fields
                    .next()
                    .ok_or_else(|| invalid("missing leap offset"))?,
            )?;
            if fields.next().is_some() || !ntp.is_multiple_of(86400) {
                return Err(invalid(
                    "leap row needs two numbers and a midnight transition",
                ));
            }
            let (year, month, day) = calendar(ntp / 86400)?;
            if day != 1 {
                return Err(invalid("leap transition must follow a month boundary"));
            }
            if let Some((last, last_offset)) = previous {
                if ntp <= last || last_offset.checked_add(1) != Some(offset) {
                    return Err(invalid("leap rows must increase with exactly +1 offsets"));
                }
                let preceding = (ntp / 86400)
                    .checked_sub(1)
                    .ok_or_else(|| invalid("leap day underflow"))?;
                let (year, month, day) = calendar(preceding)?;
                dates
                    .try_reserve(1)
                    .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
                dates.push(year * 10000 + month * 100 + day);
            } else if (ntp, offset, year, month) != (2272060800, 10, 1972, 1) {
                return Err(invalid("leap source requires the 1972 baseline offset 10"));
            }
            previous = Some((ntp, offset));
        }
    }
    let updated = updated.ok_or_else(|| invalid("missing leap update timestamp"))?;
    let expires = expires.ok_or_else(|| invalid("missing leap expiration timestamp"))?;
    let (last, _) = previous.ok_or_else(|| invalid("missing leap baseline"))?;
    if dates.is_empty()
        || dates.len() > 31
        || updated < 2272060800
        || last >= expires
        || updated >= expires
        || !expires.is_multiple_of(86400)
    {
        return Err(invalid(
            "unsupported leap count or invalid freshness ordering",
        ));
    }
    calendar(updated / 86400)?;
    calendar(expires / 86400)?;
    Ok(Table {
        dates,
        updated,
        expires,
    })
}
pub fn generate(directory: &Path) -> io::Result<String> {
    let table = read(directory)
        .and_then(|input| parse(&input))
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("{}: {error}", directory.join("leap-seconds.list").display()),
            )
        })?;
    let mut output = String::new();
    writeln!(
        output,
        "// Generated by tools/leap_generate.rs; do not edit."
    )
    .map_err(io::Error::other)?;
    writeln!(
        output,
        "// IANA leap-seconds.list; upstream source is public domain."
    )
    .map_err(io::Error::other)?;
    writeln!(output, "// SHA-256: {HASH}").map_err(io::Error::other)?;
    writeln!(
        output,
        "// Updated NTP: {}; expires NTP: {} (freshness, not validity of historical positives).",
        table.updated, table.expires
    )
    .map_err(io::Error::other)?;
    writeln!(
        output,
        "pub(super) static POSITIVE_DATES: [u32; {}] = [",
        table.dates.len()
    )
    .map_err(io::Error::other)?;
    for row in table.dates.chunks(9) {
        output.push_str("    ");
        for (index, date) in row.iter().enumerate() {
            if index != 0 {
                output.push(' ');
            }
            write!(output, "{date},").map_err(io::Error::other)?;
        }
        output.push('\n');
    }
    writeln!(output, "];").map_err(io::Error::other)?;
    Ok(output)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    const VALID: &str = "#$ 4000000000\n#@ 4023129600\n2272060800 10\n2287785600 11\n";
    #[test]
    fn numeric_dates_ignore_comments_and_validate_metadata() {
        let table = parse(&format!("{VALID}# an arbitrary comment\n")).unwrap();
        assert_eq!(table.dates, [19720630]);
        assert_eq!(table.updated, 4000000000);
        assert_eq!(table.expires, 4023129600);
        let announced = VALID.replace("4000000000", "2272060800");
        assert_eq!(parse(&announced).unwrap().dates, [19720630]);
        for expiration in ["2287785600", "2287699200"] {
            assert!(parse(&announced.replace("4023129600", expiration)).is_err());
        }
        let misleading = VALID.replace("2287785600 11", "2287785600 11 # 31 Dec 9999");
        assert_eq!(parse(&misleading).unwrap().dates, [19720630]);
        for bad in [
            VALID.replace("#$ 4000000000\n", ""),
            VALID.replace("#@ 4023129600\n", ""),
            format!("{VALID}#$ 4000000000\n"),
            format!("{VALID}#@ 4023129600\n"),
            VALID.replace("4000000000", "4023129600"),
            VALID.replace("4000000000", "1"),
            VALID.replace("4023129600", "4023129601"),
            VALID.replace("4023129600", "18446744073709551615"),
            VALID.replace("4000000000", "+4000000000"),
            VALID.replace("4000000000", "18446744073709551616"),
        ] {
            assert!(parse(&bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn five_comparison_table_capacity_is_enforced() {
        let mut source = include_str!("../leap-seconds/leap-seconds.list").to_owned();
        // Independent Jan 1 transitions in 2020..2024 extend the 27-row pin.
        for (ntp, offset) in [
            (3786825600u64, 38),
            (3818448000, 39),
            (3849984000, 40),
            (3881520000, 41),
        ] {
            writeln!(source, "{ntp} {offset}").unwrap();
        }
        assert_eq!(parse(&source).unwrap().dates.len(), 31);
        writeln!(source, "3913056000 42").unwrap();
        assert!(parse(&source)
            .unwrap_err()
            .to_string()
            .contains("unsupported leap count"));
    }
    #[test]
    fn malformed_transitions_and_negative_leaps_refuse() {
        for bad in [
            String::new(),
            VALID.replace("2272060800 10", "2272060800 11"),
            VALID.replace("2287785600 11", "2287785601 11"),
            VALID.replace("2287785600 11", "2287872000 11"),
            VALID.replace("2287785600 11", "2287785600 9"),
            VALID.replace("2287785600 11", "2287785600 12"),
            VALID.replace("2287785600 11", "2287785600 11 extra"),
            VALID.replace("2287785600 11", ""),
            format!("{VALID}2287785600 12\n"),
            format!("{VALID}2272060800 12\n"),
        ] {
            assert!(parse(&bad).is_err(), "{bad}");
        }
        assert_eq!(calendar(0).unwrap(), (1900, 1, 1));
        assert_eq!(calendar(365).unwrap(), (1901, 1, 1));
        assert_eq!(calendar(36583).unwrap(), (2000, 2, 29));
        assert!(calendar(u64::MAX).is_err());
    }
}
