//! Civil (proleptic Gregorian) date and time, feed date parsing, and a
//! TZif-backed local time zone.
//!
//! Only what a feed reader needs: seconds resolution, no calendar
//! arithmetic, no zone database beyond the one file the system points at.
//! Date/time conversion follows Howard Hinnant's `days_from_civil` /
//! `civil_from_days`; the zone reads `/etc/localtime` through [`tzif`],
//! the strict RFC 9636 TZif v2/v3 reader td-compositor's clock shares,
//! including the POSIX TZ footer, which modern "slim" files rely on for
//! every instant after their last recorded transition. It is one crate
//! td's applications depend on by path (AGENTS.md principle 2).

#![forbid(unsafe_code)]

pub mod tzif;

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds in a day.
pub const SECS_PER_DAY: i64 = 86_400;

/// Conversion is clamped to roughly +/- 1,000,000 years so a civil year
/// always fits in `i32`.
const MAX_UNIX: i64 = 32_000_000_000_000;
const MIN_UNIX: i64 = -32_000_000_000_000;

/// A wall-clock date and time with no zone attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Civil {
    pub year: i32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl Civil {
    pub const fn new(year: i32, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Civil {
        Civil {
            year,
            month,
            day,
            hour,
            minute,
            second,
        }
    }

    /// A real date and time. Leap seconds are not representable.
    pub fn is_valid(&self) -> bool {
        match days_in_month(self.year, self.month) {
            Some(last) => {
                self.day >= 1
                    && self.day <= last
                    && self.hour < 24
                    && self.minute < 60
                    && self.second < 60
            }
            None => false,
        }
    }
}

impl fmt::Display for Civil {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_ymd_hms(self))
    }
}

/// Seconds since the Unix epoch, from the system clock.
pub fn now_unix() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => i64::try_from(e.duration().as_secs())
            .map(|s| -s)
            .unwrap_or(i64::MIN),
    }
}

pub fn is_leap(year: i32) -> bool {
    tzif::leap(i64::from(year))
}

/// Length of a month, or `None` if `month` is not 1..=12.
pub fn days_in_month(year: i32, month: u8) -> Option<u8> {
    tzif::month_days(i64::from(year), month)
}

/// [`unix_to_civil_utc`], or `None` outside roughly +/- 1,000,000 years
/// instead of clamping, for a caller that must not show a wrong date.
pub fn unix_to_civil_utc_checked(unix: i64) -> Option<Civil> {
    (MIN_UNIX..=MAX_UNIX)
        .contains(&unix)
        .then(|| unix_to_civil_utc(unix))
}

/// Split a Unix timestamp into UTC civil fields. Inputs outside roughly
/// +/- 1,000,000 years are clamped.
pub fn unix_to_civil_utc(unix: i64) -> Civil {
    let unix = unix.clamp(MIN_UNIX, MAX_UNIX);
    let days = unix.div_euclid(SECS_PER_DAY);
    let secs = unix.rem_euclid(SECS_PER_DAY);
    let (year, month, day) = tzif::civil_from_days(days);
    Civil {
        year: i32::try_from(year).unwrap_or(0),
        month,
        day,
        hour: u8::try_from(secs / 3600).unwrap_or(0),
        minute: u8::try_from(secs / 60 % 60).unwrap_or(0),
        second: u8::try_from(secs % 60).unwrap_or(0),
    }
}

/// Unix timestamp for a UTC civil time, or `None` if the fields are not a
/// real date and time.
pub fn civil_to_unix_utc(c: &Civil) -> Option<i64> {
    if !c.is_valid() {
        return None;
    }
    let secs_of_day = i64::from(c.hour) * 3600 + i64::from(c.minute) * 60 + i64::from(c.second);
    i64::try_from(tzif::days_from_civil(i64::from(c.year), c.month, c.day))
        .ok()?
        .checked_mul(SECS_PER_DAY)?
        .checked_add(secs_of_day)
}

/// `%Y-%m-%d %H:%M:%S`.
pub fn format_ymd_hms(c: &Civil) -> String {
    format!("{} {}", format_ymd(c), format_hms(c))
}

/// `%Y-%m-%d`, a negative year as `-0044`.
pub fn format_ymd(c: &Civil) -> String {
    let year = if c.year < 0 {
        format!("-{:04}", i64::from(c.year).unsigned_abs())
    } else {
        format!("{:04}", c.year)
    };
    format!("{}-{:02}-{:02}", year, c.month, c.day)
}

/// `%Y-%m-%dT%H:%M:%SZ`: RFC 3339 in UTC to the second.
pub fn format_rfc3339_utc(unix: i64) -> String {
    let c = unix_to_civil_utc(unix);
    format!("{}T{}Z", format_ymd(&c), format_hms(&c))
}

/// `%H:%M:%S`.
pub fn format_hms(c: &Civil) -> String {
    format!("{:02}:{:02}:{:02}", c.hour, c.minute, c.second)
}

// ---------------------------------------------------------------- parsing

/// Leading digits: between `min` and `max` of them, with the remainder.
fn take_digits(s: &str, min: usize, max: usize) -> Option<(u32, &str)> {
    let mut val: u32 = 0;
    let mut n = 0;
    for &b in s.as_bytes().iter().take(max) {
        if !b.is_ascii_digit() {
            break;
        }
        val = val.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
        n += 1;
    }
    if n < min {
        return None;
    }
    Some((val, s.get(n..)?))
}

/// A whole token of digits.
fn all_digits(s: &str, max: usize) -> Option<u32> {
    let (v, rest) = take_digits(s, 1, max)?;
    if rest.is_empty() {
        Some(v)
    } else {
        None
    }
}

fn strip_one(s: &str, c: char) -> Option<&str> {
    s.strip_prefix(c)
}

/// `YYYY-MM-DD` with the remainder.
fn parse_date_part(s: &str) -> Option<(Civil, &str)> {
    let (y, r) = take_digits(s, 4, 4)?;
    let r = strip_one(r, '-')?;
    let (mo, r) = take_digits(r, 1, 2)?;
    let r = strip_one(r, '-')?;
    let (d, r) = take_digits(r, 1, 2)?;
    let civil = Civil {
        year: i32::try_from(y).ok()?,
        month: u8::try_from(mo).ok()?,
        day: u8::try_from(d).ok()?,
        hour: 0,
        minute: 0,
        second: 0,
    };
    Some((civil, r))
}

/// `HH:MM` and optionally `:SS`, with the remainder.
fn parse_time_part(s: &str, want_seconds: bool) -> Option<(u8, u8, u8, &str)> {
    let (h, r) = take_digits(s, 1, 2)?;
    let r = strip_one(r, ':')?;
    let (mi, r) = take_digits(r, 1, 2)?;
    let (sec, r) = if want_seconds {
        let r = strip_one(r, ':')?;
        take_digits(r, 1, 2)?
    } else {
        (0, r)
    };
    Some((
        u8::try_from(h).ok()?,
        u8::try_from(mi).ok()?,
        u8::try_from(sec).ok()?,
        r,
    ))
}

fn checked(c: Civil) -> Option<Civil> {
    if c.is_valid() {
        Some(c)
    } else {
        None
    }
}

/// `%Y-%m-%d %H:%M:%S` as a naive civil time.
pub fn parse_ymd_hms(s: &str) -> Option<Civil> {
    let (mut c, r) = parse_date_part(s.trim())?;
    let r = r.strip_prefix([' ', 'T', 't'])?;
    let (h, mi, sec, r) = parse_time_part(r, true)?;
    if !r.is_empty() {
        return None;
    }
    c.hour = h;
    c.minute = mi;
    c.second = sec;
    checked(c)
}

/// `%Y-%m-%d %H:%M` as a naive civil time.
pub fn parse_ymd_hm(s: &str) -> Option<Civil> {
    let (mut c, r) = parse_date_part(s.trim())?;
    let r = r.strip_prefix([' ', 'T', 't'])?;
    let (h, mi, _, r) = parse_time_part(r, false)?;
    if !r.is_empty() {
        return None;
    }
    c.hour = h;
    c.minute = mi;
    checked(c)
}

/// `%Y-%m-%d` at midnight.
pub fn parse_ymd(s: &str) -> Option<Civil> {
    let (c, r) = parse_date_part(s.trim())?;
    if !r.is_empty() {
        return None;
    }
    checked(c)
}

/// `Z`, `z`, `+hh:mm`, `-hhmm` or `+hh`, consuming the whole string.
fn parse_offset(s: &str) -> Option<i32> {
    if s.eq_ignore_ascii_case("z") {
        return Some(0);
    }
    let (sign, r) = match s.as_bytes().first() {
        Some(b'+') => (1, s.get(1..)?),
        Some(b'-') => (-1, s.get(1..)?),
        _ => return None,
    };
    let (h, r) = take_digits(r, 2, 2)?;
    let r = r.strip_prefix(':').unwrap_or(r);
    let m = if r.is_empty() {
        0
    } else {
        let (m, rest) = take_digits(r, 2, 2)?;
        if !rest.is_empty() {
            return None;
        }
        m
    };
    if h > 23 || m > 59 {
        return None;
    }
    Some(sign * (i32::try_from(h).ok()? * 3600 + i32::try_from(m).ok()? * 60))
}

/// RFC 3339 timestamp to a Unix timestamp. Fractional seconds are dropped
/// and a leap second (`:60`) is folded onto `:59`.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim();
    let (mut c, r) = parse_date_part(s)?;
    let r = r.strip_prefix(['T', 't', ' '])?;
    let (h, mi, sec, r) = parse_time_part(r, false)?;
    // Seconds are mandatory in RFC 3339 but feeds omit them.
    let (sec, r) = match r.strip_prefix(':') {
        Some(rest) => {
            let (v, rest) = take_digits(rest, 2, 2)?;
            (u8::try_from(v).ok()?, rest)
        }
        None => (sec, r),
    };
    let r = match r.strip_prefix('.') {
        Some(rest) => {
            let (_, rest) = take_digits(rest, 1, 9)?;
            rest
        }
        None => r,
    };
    let off = parse_offset(r)?;
    c.hour = h;
    c.minute = mi;
    c.second = sec.min(59);
    civil_to_unix_utc(&c)?.checked_sub(i64::from(off))
}

/// Drop RFC 5322 comments and commas so the rest can be tokenized.
fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    let mut escaped = false;
    for ch in s.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if depth > 0 => escaped = true,
            '(' => {
                depth += 1;
                out.push(' ');
            }
            ')' => {
                depth = depth.saturating_sub(1);
                out.push(' ');
            }
            ',' if depth == 0 => out.push(' '),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out
}

fn is_day_name(tok: &str) -> bool {
    const DAYS: [&str; 14] = [
        "mon",
        "tue",
        "wed",
        "thu",
        "fri",
        "sat",
        "sun",
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
    ];
    let lower = tok.trim_end_matches('.').to_ascii_lowercase();
    DAYS.contains(&lower.as_str())
}

fn month_num(tok: &str) -> Option<u8> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let lower = tok.to_ascii_lowercase();
    let head = lower.get(..3)?;
    MONTHS
        .iter()
        .position(|m| *m == head)
        .and_then(|i| u8::try_from(i + 1).ok())
}

/// RFC 5322 obsolete year forms: two digits pivot at 50, three digits are
/// offsets from 1900.
fn rfc2822_year(tok: &str) -> Option<i32> {
    let digits = tok.len();
    let v = i32::try_from(all_digits(tok, 4)?).ok()?;
    match digits {
        2 => Some(if v < 50 { 2000 + v } else { 1900 + v }),
        3 => Some(1900 + v),
        4 => Some(v),
        _ => None,
    }
}

/// Numeric zone, the named zones RFC 5322 lists, or a single military
/// letter (defined as "unknown", so UTC with no offset applied).
fn rfc2822_zone(tok: &str) -> Option<i32> {
    if matches!(tok.as_bytes().first(), Some(b'+') | Some(b'-')) {
        let (sign, r) = match tok.as_bytes().first() {
            Some(b'-') => (-1, tok.get(1..)?),
            _ => (1, tok.get(1..)?),
        };
        let (h, r) = take_digits(r, 1, 2)?;
        let r = r.strip_prefix(':').unwrap_or(r);
        let (m, r) = take_digits(r, 2, 2)?;
        if !r.is_empty() || h > 23 || m > 59 {
            return None;
        }
        return Some(sign * (i32::try_from(h).ok()? * 3600 + i32::try_from(m).ok()? * 60));
    }
    let upper = tok.to_ascii_uppercase();
    let hours = match upper.as_str() {
        "UT" | "UTC" | "GMT" => 0,
        "EST" => -5,
        "EDT" => -4,
        "CST" => -6,
        "CDT" => -5,
        "MST" => -7,
        "MDT" => -6,
        "PST" => -8,
        "PDT" => -7,
        _ => {
            let one_letter = upper.len() == 1
                && matches!(upper.as_bytes().first(), Some(b) if b.is_ascii_alphabetic());
            if one_letter {
                0
            } else {
                return None;
            }
        }
    };
    Some(hours * 3600)
}

/// RFC 2822 / 5322 date to a Unix timestamp. The day name, seconds and the
/// zone are optional; a missing zone is read as UTC.
pub fn parse_rfc2822(s: &str) -> Option<i64> {
    let cleaned = strip_comments(s);
    let toks: Vec<&str> = cleaned.split_whitespace().collect();
    let mut i = 0;
    if matches!(toks.first(), Some(t) if is_day_name(t)) {
        i = 1;
    }
    let day = u8::try_from(all_digits(toks.get(i)?, 2)?).ok()?;
    let month = month_num(toks.get(i + 1)?)?;
    let year = rfc2822_year(toks.get(i + 2)?)?;
    let time = toks.get(i + 3)?;
    let (h, mi, sec, rest) = match parse_time_part(time, true) {
        Some(v) => v,
        None => {
            let (h, mi, _, rest) = parse_time_part(time, false)?;
            (h, mi, 0, rest)
        }
    };
    if !rest.is_empty() {
        return None;
    }
    let off = match toks.get(i + 4) {
        Some(z) => rfc2822_zone(z)?,
        None => 0,
    };
    let c = Civil {
        year,
        month,
        day,
        hour: h,
        minute: mi,
        second: sec.min(59),
    };
    civil_to_unix_utc(&c)?.checked_sub(i64::from(off))
}

// ------------------------------------------------------------------ zones

/// A time zone: a TZif file [`tzif::Zone::parse`] accepted, or UTC.
#[derive(Debug)]
pub struct Zone {
    rules: Option<tzif::Zone>,
}

impl Zone {
    /// UTC, used whenever the system zone cannot be read.
    pub fn utc() -> Zone {
        Zone { rules: None }
    }

    /// The system zone, or UTC if it cannot be read or parsed.
    pub fn local() -> Zone {
        let tz = std::env::var("TZ").ok();
        Zone::from_file(&tz_file_path(tz.as_deref()))
    }

    /// The zone a file holds, or UTC if it cannot be read or parsed.
    fn from_file(path: &str) -> Zone {
        read_zone_file(path)
            .and_then(|data| Zone::from_tzif(&data))
            .unwrap_or_else(Zone::utc)
    }

    /// A TZif v2 or v3 file (RFC 9636), held to [`tzif::Zone::parse`]'s
    /// bounds and checks.
    pub fn from_tzif(data: &[u8]) -> Option<Zone> {
        tzif::Zone::parse(data).map(|rules| Zone { rules: Some(rules) })
    }

    /// Seconds east of UTC at an instant: UTC where the file gives no
    /// offset, a range it leaves unknown or its `-00` placeholder.
    pub fn offset_at(&self, unix: i64) -> i32 {
        self.rules
            .as_ref()
            .map_or(Some(0), |rules| rules.offset_at(unix))
            .unwrap_or(0)
    }

    /// Local civil time at an instant, with the offset used.
    pub fn to_local(&self, unix: i64) -> (Civil, i32) {
        let off = self.offset_at(unix);
        (unix_to_civil_utc(unix.saturating_add(i64::from(off))), off)
    }

    /// Every offset `offset_at` can answer, UTC's included.
    fn candidate_offsets(&self) -> Vec<i32> {
        let mut offs = self
            .rules
            .as_ref()
            .map_or_else(Vec::new, tzif::Zone::offsets);
        offs.push(0);
        offs.sort_unstable();
        offs.dedup();
        offs
    }

    /// Instants whose local time is `c`: none in a spring-forward gap, two
    /// in a fall-back overlap.
    fn local_candidates(&self, c: &Civil) -> Vec<i64> {
        let Some(local) = civil_to_unix_utc(c) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for off in self.candidate_offsets() {
            let Some(unix) = local.checked_sub(i64::from(off)) else {
                continue;
            };
            if self.offset_at(unix) == off {
                found.push(unix);
            }
        }
        found.sort_unstable();
        found.dedup();
        found
    }

    /// The instant for a local civil time, or `None` when it is ambiguous
    /// or does not exist.
    pub fn from_local_single(&self, c: &Civil) -> Option<i64> {
        let found = self.local_candidates(c);
        match found.len() {
            1 => found.first().copied(),
            _ => None,
        }
    }

    /// The earliest instant for a local civil time; `None` if it does not
    /// exist.
    pub fn from_local_earliest(&self, c: &Civil) -> Option<i64> {
        self.local_candidates(c).first().copied()
    }
}

/// A regular file's bytes, at most one past [`tzif::MAX_BYTES`] so an
/// oversized file still refuses in the parser.
fn read_zone_file(path: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let limit = u64::try_from(tzif::MAX_BYTES).ok()?.checked_add(1)?;
    let mut data = Vec::new();
    file.take(limit).read_to_end(&mut data).ok()?;
    Some(data)
}

/// `$TZ` is honoured only when it names a file (`:/path` or `/path`).
fn tz_file_path(tz: Option<&str>) -> String {
    if let Some(value) = tz {
        let path = value.strip_prefix(':').unwrap_or(value);
        if path.starts_with('/') {
            return path.to_string();
        }
    }
    "/etc/localtime".to_string()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn utc_formats_render_the_date_and_the_rfc3339_instant() {
        assert_eq!(format_ymd(&civil(2024, 1, 2, 3, 4, 5)), "2024-01-02");
        assert_eq!(format_ymd(&civil(-1, 12, 31, 0, 0, 0)), "-0001-12-31");
        assert_eq!(format_rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339_utc(-1), "1969-12-31T23:59:59Z");
        assert_eq!(format_rfc3339_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_rfc3339_utc(4_107_542_399), "2100-02-28T23:59:59Z");
    }

    #[test]
    fn the_checked_split_refuses_what_the_plain_one_clamps() {
        assert_eq!(unix_to_civil_utc_checked(0), Some(unix_to_civil_utc(0)));
        assert_eq!(
            unix_to_civil_utc_checked(MAX_UNIX),
            Some(unix_to_civil_utc(MAX_UNIX))
        );
        assert_eq!(
            unix_to_civil_utc_checked(MIN_UNIX),
            Some(unix_to_civil_utc(MIN_UNIX))
        );
        assert_eq!(unix_to_civil_utc_checked(MAX_UNIX + 1), None);
        assert_eq!(unix_to_civil_utc_checked(MIN_UNIX - 1), None);
        assert_eq!(unix_to_civil_utc_checked(i64::MIN), None);
    }

    fn civil(y: i32, mo: u8, d: u8, h: u8, mi: u8, s: u8) -> Civil {
        Civil::new(y, mo, d, h, mi, s)
    }

    fn unix(y: i32, mo: u8, d: u8, h: u8, mi: u8, s: u8) -> i64 {
        civil_to_unix_utc(&civil(y, mo, d, h, mi, s)).expect("valid civil time")
    }

    #[test]
    fn known_epoch_and_civil_pairs() {
        let pairs = [
            (0i64, civil(1970, 1, 1, 0, 0, 0)),
            (-1, civil(1969, 12, 31, 23, 59, 59)),
            (1_234_567_890, civil(2009, 2, 13, 23, 31, 30)),
            (951_782_400, civil(2000, 2, 29, 0, 0, 0)),
            (2_147_483_647, civil(2038, 1, 19, 3, 14, 7)),
            (-2_208_988_800, civil(1900, 1, 1, 0, 0, 0)),
            (4_102_444_800, civil(2100, 1, 1, 0, 0, 0)),
            (-62_135_596_800, civil(1, 1, 1, 0, 0, 0)),
        ];
        for (secs, want) in pairs {
            assert_eq!(unix_to_civil_utc(secs), want, "at {}", secs);
            assert_eq!(civil_to_unix_utc(&want), Some(secs), "at {}", secs);
        }
    }

    #[test]
    fn century_leap_rules() {
        assert!(is_leap(2000));
        assert!(is_leap(2024));
        assert!(!is_leap(1900));
        assert!(!is_leap(2100));
        assert_eq!(civil_to_unix_utc(&civil(1900, 2, 29, 0, 0, 0)), None);
        assert_eq!(civil_to_unix_utc(&civil(2100, 2, 29, 0, 0, 0)), None);
        assert!(civil_to_unix_utc(&civil(2000, 2, 29, 0, 0, 0)).is_some());
        // 1900 and 2100 skip a day where 2000 does not.
        assert_eq!(
            unix(1900, 3, 1, 0, 0, 0) - unix(1900, 2, 28, 0, 0, 0),
            SECS_PER_DAY
        );
        assert_eq!(
            unix(2100, 3, 1, 0, 0, 0) - unix(2100, 2, 28, 0, 0, 0),
            SECS_PER_DAY
        );
        assert_eq!(
            unix(2000, 3, 1, 0, 0, 0) - unix(2000, 2, 28, 0, 0, 0),
            2 * SECS_PER_DAY
        );
    }

    #[test]
    fn conversion_round_trips_across_centuries() {
        let mut t = unix(1600, 1, 1, 0, 0, 0);
        let end = unix(2400, 1, 1, 0, 0, 0);
        let step = SECS_PER_DAY * 37 + 3607;
        while t < end {
            let c = unix_to_civil_utc(t);
            assert!(c.is_valid(), "invalid civil at {}", t);
            assert_eq!(civil_to_unix_utc(&c), Some(t), "round trip at {}", t);
            t += step;
        }
    }

    #[test]
    fn invalid_civil_fields_are_rejected() {
        for bad in [
            civil(2024, 0, 1, 0, 0, 0),
            civil(2024, 13, 1, 0, 0, 0),
            civil(2024, 4, 31, 0, 0, 0),
            civil(2024, 1, 0, 0, 0, 0),
            civil(2024, 1, 1, 24, 0, 0),
            civil(2024, 1, 1, 0, 60, 0),
            civil(2024, 1, 1, 0, 0, 60),
        ] {
            assert!(!bad.is_valid(), "{:?} should be invalid", bad);
            assert_eq!(civil_to_unix_utc(&bad), None);
        }
    }

    #[test]
    fn out_of_range_instants_are_clamped_not_panicking() {
        let c = unix_to_civil_utc(i64::MAX);
        assert!(c.is_valid());
        let c = unix_to_civil_utc(i64::MIN);
        assert!(c.is_valid());
    }

    #[test]
    fn formatting_pads_and_handles_negative_years() {
        assert_eq!(
            format_ymd_hms(&civil(2024, 1, 2, 3, 4, 5)),
            "2024-01-02 03:04:05"
        );
        assert_eq!(format_hms(&civil(2024, 1, 2, 3, 4, 5)), "03:04:05");
        assert_eq!(
            format_ymd_hms(&civil(-44, 3, 15, 12, 0, 0)),
            "-0044-03-15 12:00:00"
        );
        assert_eq!(
            civil(2024, 1, 2, 3, 4, 5).to_string(),
            "2024-01-02 03:04:05"
        );
    }

    #[test]
    fn now_is_in_a_plausible_range() {
        let now = now_unix();
        assert!(
            now > unix(2020, 1, 1, 0, 0, 0),
            "clock before 2020: {}",
            now
        );
        assert!(now < unix(2200, 1, 1, 0, 0, 0), "clock after 2200: {}", now);
    }

    #[test]
    fn rfc3339_corpus() {
        let cases = [
            ("1985-04-12T23:20:50.52Z", 482_196_050i64),
            ("1996-12-19T16:39:57-08:00", 851_042_397),
            ("1996-12-20T00:39:57Z", 851_042_397),
            ("1990-12-31T23:59:60Z", 662_687_999),
            ("2024-01-01T00:00:00Z", 1_704_067_200),
            ("2024-01-01t00:00:00z", 1_704_067_200),
            ("2024-01-01 00:00:00Z", 1_704_067_200),
            ("2024-01-01T05:30:00+05:30", 1_704_067_200),
            ("2023-12-31T19:00:00-0500", 1_704_067_200),
            ("2024-01-01T00:00:00.123456789Z", 1_704_067_200),
            ("2024-01-01T00:00Z", 1_704_067_200),
            ("  2024-01-01T00:00:00Z  ", 1_704_067_200),
            ("2024-02-29T12:00:00Z", 1_709_208_000),
        ];
        for (input, want) in cases {
            assert_eq!(parse_rfc3339(input), Some(want), "parsing {}", input);
        }
    }

    #[test]
    fn rfc3339_rejects_nonsense() {
        for bad in [
            "",
            "2024-01-01",
            "2024-13-01T00:00:00Z",
            "2024-01-32T00:00:00Z",
            "2024-01-01T00:00:00",
            "2024-01-01T00:00:00+25:00",
            "2024-01-01T00:00:00Zjunk",
            "Mon, 01 Jan 2024 00:00:00 GMT",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "should reject {:?}", bad);
        }
    }

    #[test]
    fn rfc2822_corpus() {
        let cases = [
            ("Mon, 01 Jan 2024 00:00:00 GMT", 1_704_067_200i64),
            ("Mon, 1 Jan 2024 00:00:00 +0000", 1_704_067_200),
            ("1 Jan 2024 00:00:00 +0000", 1_704_067_200),
            ("Sun, 31 Dec 2023 19:00:00 -0500", 1_704_067_200),
            ("Mon, 01 Jan 2024 00:00 UT", 1_704_067_200),
            ("Mon, 01 Jan 2024 00:00:00 +0000 (UTC)", 1_704_067_200),
            ("Sun, 06 Nov 1994 08:49:37 GMT", 784_111_777),
            ("Sun, 06 Nov 1994 03:49:37 EST", 784_111_777),
            ("Sun, 06 Nov 1994 02:49:37 CST", 784_111_777),
            ("Sun, 06 Nov 1994 01:49:37 MST", 784_111_777),
            ("Sun, 06 Nov 1994 00:49:37 PST", 784_111_777),
            ("Sun, 06 Nov 1994 04:49:37 EDT", 784_111_777),
            ("Sun, 06 Nov 1994 08:49:37 A", 784_111_777),
            ("Thu, 01 Jan 04 00:00:00 GMT", 1_072_915_200),
            ("Fri, 01 Jan 99 00:00:00 GMT", 915_148_800),
            ("Sat, 01 Jan 100 00:00:00 GMT", 946_684_800),
            ("Thu, 29 Feb 2024 12:00:00 GMT", 1_709_208_000),
            ("Mon, 01 Jan 2024 00:00:00 +05:30", 1_704_047_400),
        ];
        for (input, want) in cases {
            assert_eq!(parse_rfc2822(input), Some(want), "parsing {}", input);
        }
    }

    #[test]
    fn rfc2822_rejects_nonsense() {
        for bad in [
            "",
            "Mon, 01 Xxx 2024 00:00:00 GMT",
            "Mon, 32 Jan 2024 00:00:00 GMT",
            "Mon, 01 Jan 2024 25:00:00 GMT",
            "Mon, 01 Jan 2024",
            "Mon, 01 Jan 2024 00:00:00 NOPE",
            "2024-01-01T00:00:00Z",
        ] {
            assert_eq!(parse_rfc2822(bad), None, "should reject {:?}", bad);
        }
    }

    #[test]
    fn naive_formats() {
        assert_eq!(
            parse_ymd_hms("2024-03-10 02:30:00"),
            Some(civil(2024, 3, 10, 2, 30, 0))
        );
        assert_eq!(
            parse_ymd_hms(" 2024-3-1 2:3:4 "),
            Some(civil(2024, 3, 1, 2, 3, 4))
        );
        assert_eq!(
            parse_ymd_hm("2024-03-10 02:30"),
            Some(civil(2024, 3, 10, 2, 30, 0))
        );
        assert_eq!(parse_ymd("2024-03-10"), Some(civil(2024, 3, 10, 0, 0, 0)));
        for bad in ["2024-03-10 02:30", "2024-03-10", "", "2024-02-30 00:00:00"] {
            assert_eq!(parse_ymd_hms(bad), None, "should reject {:?}", bad);
        }
        for bad in ["2024-03-10 02:30:00", "2024-03-10"] {
            assert_eq!(parse_ymd_hm(bad), None, "should reject {:?}", bad);
        }
        for bad in ["2024-03-10 02:30", "24-03-10", "2024-02-30"] {
            assert_eq!(parse_ymd(bad), None, "should reject {:?}", bad);
        }
    }

    // ---- zones -----------------------------------------------------------
    //
    // The TZif parser and the POSIX footer grammar are tzif.rs's, tested
    // there; these hold the zone's local-time behaviour over it.

    use crate::tzif::fixture;

    /// US Eastern for 2024, with a footer covering everything after.
    fn eastern() -> Zone {
        Zone::from_tzif(&fixture(
            &[
                (unix(2024, 3, 10, 7, 0, 0), 1),
                (unix(2024, 11, 3, 6, 0, 0), 0),
            ],
            &[(-18_000, false, "EST"), (-14_400, true, "EDT")],
            "EST5EDT,M3.2.0,M11.1.0",
        ))
        .expect("fixture parses")
    }

    #[test]
    fn to_local_before_between_and_after_transitions() {
        let z = eastern();
        // Before the first transition the first type applies.
        assert_eq!(
            z.to_local(unix(2024, 1, 15, 12, 0, 0)),
            (civil(2024, 1, 15, 7, 0, 0), -18_000)
        );
        // Between them, the recorded DST type.
        assert_eq!(
            z.to_local(unix(2024, 6, 1, 12, 0, 0)),
            (civil(2024, 6, 1, 8, 0, 0), -14_400)
        );
        // After the last transition the footer governs, in both seasons.
        assert_eq!(
            z.to_local(unix(2024, 12, 1, 12, 0, 0)),
            (civil(2024, 12, 1, 7, 0, 0), -18_000)
        );
        assert_eq!(
            z.to_local(unix(2025, 7, 4, 16, 0, 0)),
            (civil(2025, 7, 4, 12, 0, 0), -14_400)
        );
        assert_eq!(
            z.to_local(unix(2030, 3, 10, 12, 0, 0)),
            (civil(2030, 3, 10, 8, 0, 0), -14_400)
        );
    }

    #[test]
    fn from_local_at_a_gap_and_an_overlap() {
        let z = eastern();
        // Unambiguous.
        assert_eq!(
            z.from_local_single(&civil(2024, 6, 1, 8, 0, 0)),
            Some(unix(2024, 6, 1, 12, 0, 0))
        );
        assert_eq!(
            z.from_local_earliest(&civil(2024, 6, 1, 8, 0, 0)),
            Some(unix(2024, 6, 1, 12, 0, 0))
        );
        // Spring forward: 02:30 never happens.
        assert_eq!(z.from_local_single(&civil(2024, 3, 10, 2, 30, 0)), None);
        assert_eq!(z.from_local_earliest(&civil(2024, 3, 10, 2, 30, 0)), None);
        // Fall back: 01:30 happens twice, earliest is the DST one.
        assert_eq!(z.from_local_single(&civil(2024, 11, 3, 1, 30, 0)), None);
        assert_eq!(
            z.from_local_earliest(&civil(2024, 11, 3, 1, 30, 0)),
            Some(unix(2024, 11, 3, 5, 30, 0))
        );
    }

    #[test]
    fn footer_era_gap_and_overlap_behave_the_same() {
        let z = eastern();
        assert_eq!(z.from_local_single(&civil(2025, 3, 9, 2, 30, 0)), None);
        assert_eq!(z.from_local_earliest(&civil(2025, 3, 9, 2, 30, 0)), None);
        assert_eq!(z.from_local_single(&civil(2025, 11, 2, 1, 30, 0)), None);
        assert_eq!(
            z.from_local_earliest(&civil(2025, 11, 2, 1, 30, 0)),
            Some(unix(2025, 11, 2, 5, 30, 0))
        );
    }

    #[test]
    fn a_footer_only_zone_needs_no_transitions() {
        let z = Zone::from_tzif(&fixture(&[], &[(12_600, false, "+0330")], "<+0330>-3:30"))
            .expect("parses");
        assert_eq!(z.offset_at(unix(2024, 1, 1, 0, 0, 0)), 12_600);
        assert_eq!(
            z.to_local(unix(2024, 1, 1, 0, 0, 0)),
            (civil(2024, 1, 1, 3, 30, 0), 12_600)
        );
    }

    #[test]
    fn local_round_trip_holds_across_two_years() {
        let z = eastern();
        let mut t = unix(2024, 1, 1, 0, 0, 0);
        let end = unix(2026, 1, 1, 0, 0, 0);
        let mut ambiguous = 0;
        while t < end {
            let (c, _) = z.to_local(t);
            let back = z
                .from_local_earliest(&c)
                .unwrap_or_else(|| panic!("{} has no instant", c));
            assert!(back <= t, "{} went forward", c);
            assert_eq!(z.to_local(back).0, c, "{} did not round trip", c);
            match z.from_local_single(&c) {
                Some(single) => assert_eq!(single, t, "{} moved", c),
                None => {
                    // The fall-back hour: two instants, an hour apart.
                    ambiguous += 1;
                    assert!(back == t || back == t - 3600, "{} is not an overlap", c);
                }
            }
            t += 900;
        }
        // One overlapping hour, at quarter-hour steps, in each of two years.
        assert_eq!(ambiguous, 16);
    }

    #[test]
    fn southern_footer_rules_span_the_new_year() {
        let z = Zone::from_tzif(&fixture(
            &[],
            &[(43_200, false, "NZST")],
            "NZST-12NZDT,M9.5.0,M4.1.0/3",
        ))
        .expect("parses");
        assert_eq!(z.offset_at(unix(2024, 1, 15, 0, 0, 0)), 46_800);
        assert_eq!(z.offset_at(unix(2024, 6, 15, 0, 0, 0)), 43_200);
        assert_eq!(z.offset_at(unix(2024, 12, 15, 0, 0, 0)), 46_800);
    }

    #[test]
    fn an_unknown_range_reads_as_utc_and_inverts_consistently() {
        // No footer: after the last transition the file says nothing.
        let z = Zone::from_tzif(&fixture(
            &[(unix(2024, 1, 1, 0, 0, 0), 0)],
            &[(3600, false, "CET")],
            "",
        ))
        .expect("parses");
        assert_eq!(z.offset_at(unix(2023, 6, 1, 0, 0, 0)), 3600);
        assert_eq!(z.offset_at(unix(2024, 6, 1, 0, 0, 0)), 0);
        assert_eq!(
            z.from_local_single(&civil(2024, 6, 1, 0, 0, 0)),
            Some(unix(2024, 6, 1, 0, 0, 0))
        );
        let placeholder =
            Zone::from_tzif(&fixture(&[], &[(0, false, "-00")], "<-00>0")).expect("parses");
        assert_eq!(placeholder.to_local(0), (civil(1970, 1, 1, 0, 0, 0), 0));
    }

    #[test]
    fn what_the_strict_reader_refuses_falls_back_to_utc() {
        let valid = fixture(&[], &[(0, false, "UTC")], "UTC0");
        assert!(Zone::from_tzif(&valid).is_some());
        // Version 1 alone, an empty 32-bit block (RFC 9636 3.1: typecnt
        // and charcnt MUST NOT be zero), a leap-second table, and a footer
        // the last transition contradicts.
        let mut v1 = valid.clone();
        v1[4] = 0;
        let mut empty = Vec::new();
        empty.extend_from_slice(b"TZif3");
        empty.extend_from_slice(&[0; 39]);
        empty.extend_from_slice(&valid[51..]);
        let mut leap = valid.clone();
        leap[51 + 28..51 + 32].copy_from_slice(&1u32.to_be_bytes());
        leap.splice(105..105, [0; 12]);
        let contradicted = fixture(&[(100, 0)], &[(3600, false, "UTC")], "UTC0");
        for bad in [
            &b""[..],
            b"TZif",
            b"nope\0garbage",
            &valid[..4],
            &v1,
            &empty,
            &leap,
            &contradicted,
            &vec![0; tzif::MAX_BYTES + 1],
        ] {
            assert!(Zone::from_tzif(bad).is_none(), "should reject {:?}", bad);
        }
        let z = Zone::utc();
        assert_eq!(z.offset_at(1_704_067_200), 0);
        assert_eq!(z.to_local(1_704_067_200), (civil(2024, 1, 1, 0, 0, 0), 0));
        assert_eq!(
            z.from_local_single(&civil(2024, 1, 1, 0, 0, 0)),
            Some(1_704_067_200)
        );
    }

    #[test]
    fn local_zone_loads_without_panicking() {
        let z = Zone::local();
        let (c, off) = z.to_local(1_704_067_200);
        assert!(c.is_valid());
        assert!((-93_599..=93_599).contains(&off));
    }

    #[test]
    fn a_zone_file_is_read_whole_and_bounded() {
        assert_eq!(read_zone_file("/nonexistent/zone"), None);
        assert_eq!(read_zone_file("/"), None);
        assert_eq!(read_zone_file("/dev/null"), None);
        let dir = std::env::temp_dir().join(format!("td-civil-zone-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name).to_str().unwrap().to_owned();
        let valid = fixture(&[], &[(19_800, false, "IST")], "IST-5:30");
        std::fs::write(path("valid"), &valid).unwrap();
        std::fs::write(path("malformed"), b"TZif3 not really").unwrap();
        std::fs::write(path("oversized"), vec![b'x'; tzif::MAX_BYTES + 10]).unwrap();
        assert_eq!(read_zone_file(&path("valid")), Some(valid));
        assert_eq!(
            read_zone_file(&path("oversized")).map(|data| data.len()),
            Some(tzif::MAX_BYTES + 1)
        );
        // `local`'s path: a readable zone is used, anything else is UTC.
        assert_eq!(Zone::from_file(&path("valid")).offset_at(0), 19_800);
        for name in ["malformed", "oversized", "missing"] {
            assert_eq!(Zone::from_file(&path(name)).offset_at(0), 0, "{name}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tz_env_is_honoured_only_as_a_path() {
        assert_eq!(
            tz_file_path(Some(":/usr/share/zoneinfo/UTC")),
            "/usr/share/zoneinfo/UTC"
        );
        assert_eq!(tz_file_path(Some("/etc/foo")), "/etc/foo");
        assert_eq!(tz_file_path(Some("Europe/Paris")), "/etc/localtime");
        assert_eq!(tz_file_path(Some(":Europe/Paris")), "/etc/localtime");
        assert_eq!(tz_file_path(Some("EST5EDT")), "/etc/localtime");
        assert_eq!(tz_file_path(None), "/etc/localtime");
    }
}
