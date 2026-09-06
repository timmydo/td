//! Civil (proleptic Gregorian) date and time, feed date parsing, and a
//! TZif-backed local time zone.
//!
//! Only what a feed reader needs: seconds resolution, no calendar
//! arithmetic, no zone database beyond the one file the system points at.
//! Date/time conversion follows Howard Hinnant's `days_from_civil` /
//! `civil_from_days`; the zone reads `/etc/localtime` (RFC 8536 TZif v1, v2
//! and v3) including the POSIX TZ footer, which modern "slim" files rely on
//! for every instant after their last recorded transition.

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
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Length of a month, or `None` if `month` is not 1..=12.
pub fn days_in_month(year: i32, month: u8) -> Option<u8> {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => Some(31),
        4 | 6 | 9 | 11 => Some(30),
        2 => Some(if is_leap(year) { 29 } else { 28 }),
        _ => None,
    }
}

/// Days since 1970-01-01 (Hinnant). Valid for any proleptic Gregorian date.
fn days_from_civil(year: i32, month: u8, day: u8) -> i64 {
    let m = i64::from(month);
    let y = i64::from(year) - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`]; the year is returned wide so the caller
/// decides how to clamp it.
fn civil_from_days(days: i64) -> (i64, u8, u8) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (
        year,
        u8::try_from(m).unwrap_or(1),
        u8::try_from(d).unwrap_or(1),
    )
}

/// Split a Unix timestamp into UTC civil fields. Inputs outside roughly
/// +/- 1,000,000 years are clamped.
pub fn unix_to_civil_utc(unix: i64) -> Civil {
    let unix = unix.clamp(MIN_UNIX, MAX_UNIX);
    let days = unix.div_euclid(SECS_PER_DAY);
    let secs = unix.rem_euclid(SECS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
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
    days_from_civil(c.year, c.month, c.day)
        .checked_mul(SECS_PER_DAY)?
        .checked_add(secs_of_day)
}

/// `%Y-%m-%d %H:%M:%S`.
pub fn format_ymd_hms(c: &Civil) -> String {
    let year = if c.year < 0 {
        format!("-{:04}", i64::from(c.year).unsigned_abs())
    } else {
        format!("{:04}", c.year)
    };
    format!(
        "{}-{:02}-{:02} {:02}:{:02}:{:02}",
        year, c.month, c.day, c.hour, c.minute, c.second
    )
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

/// One TZif local time type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LocalType {
    /// Seconds east of UTC.
    utoff: i32,
    isdst: bool,
}

/// A POSIX TZ rule day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// `Jn`: day n of the year, never counting February 29.
    Julian(u16),
    /// `n`: zero-based day of the year, February 29 included.
    ZeroDay(u16),
    /// `Mm.w.d`: weekday `d` of week `w` (5 means last) in month `m`.
    Month { m: u8, w: u8, d: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuleTime {
    rule: Rule,
    /// Local seconds after midnight; may be negative or beyond a day.
    time: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Dst {
    ty: LocalType,
    start: RuleTime,
    end: RuleTime,
}

/// A parsed POSIX TZ string, as found in a TZif footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PosixTz {
    std: LocalType,
    dst: Option<Dst>,
}

/// Days since the epoch for a rule's day in `year`.
fn rule_day(year: i32, rule: Rule) -> i64 {
    match rule {
        Rule::Julian(n) => {
            let n = i64::from(n);
            let extra = i64::from(is_leap(year) && n > 59);
            days_from_civil(year, 1, 1) + n - 1 + extra
        }
        Rule::ZeroDay(n) => days_from_civil(year, 1, 1) + i64::from(n),
        Rule::Month { m, w, d } => {
            let first = days_from_civil(year, m, 1);
            // 1970-01-01 was a Thursday; 0 is Sunday.
            let weekday = (first + 4).rem_euclid(7);
            let mut day = (i64::from(d) - weekday).rem_euclid(7) + (i64::from(w) - 1) * 7;
            let last = i64::from(days_in_month(year, m).unwrap_or(28));
            while day >= last {
                day -= 7;
            }
            first + day
        }
    }
}

impl PosixTz {
    /// The type in effect at a UTC instant.
    fn type_at(&self, unix: i64) -> LocalType {
        let Some(dst) = self.dst else {
            return self.std;
        };
        // The year is taken from local standard time, as tzcode does.
        let year = unix_to_civil_utc(unix.saturating_add(i64::from(self.std.utoff))).year;
        let start = self.instant(year, dst.start, self.std.utoff);
        let end = self.instant(year, dst.end, dst.ty.utoff);
        let in_dst = if start <= end {
            unix >= start && unix < end
        } else {
            // Southern hemisphere: DST spans the new year.
            unix >= start || unix < end
        };
        if in_dst {
            dst.ty
        } else {
            self.std
        }
    }

    /// UTC instant of a rule in `year`. The rule's time is local time in the
    /// offset in effect just before the change.
    fn instant(&self, year: i32, rt: RuleTime, before: i32) -> i64 {
        rule_day(year, rt.rule)
            .saturating_mul(SECS_PER_DAY)
            .saturating_add(i64::from(rt.time))
            .saturating_sub(i64::from(before))
    }
}

/// A zone name: `<...>` quoted or a run of letters.
fn skip_tz_name(s: &str) -> Option<&str> {
    if let Some(r) = s.strip_prefix('<') {
        let end = r.find('>')?;
        if end == 0 {
            return None;
        }
        r.get(end + 1..)
    } else {
        let n = s
            .as_bytes()
            .iter()
            .take_while(|&&b| b.is_ascii_alphabetic())
            .count();
        if n == 0 {
            return None;
        }
        s.get(n..)
    }
}

/// `[+|-]hh[:mm[:ss]]`. POSIX counts west as positive; the result is the
/// usual seconds-east offset.
fn parse_tz_utoff(s: &str) -> Option<(i32, &str)> {
    let (sign, r) = match s.as_bytes().first() {
        Some(b'+') => (1, s.get(1..)?),
        Some(b'-') => (-1, s.get(1..)?),
        _ => (1, s),
    };
    let (h, r) = take_digits(r, 1, 3)?;
    if h > 167 {
        return None;
    }
    let (m, r) = match r.strip_prefix(':') {
        Some(rest) => take_digits(rest, 1, 2)?,
        None => (0, r),
    };
    let (sec, r) = match r.strip_prefix(':') {
        Some(rest) => take_digits(rest, 1, 2)?,
        None => (0, r),
    };
    if m > 59 || sec > 59 {
        return None;
    }
    let total = i32::try_from(h * 3600 + m * 60 + sec).ok()?;
    Some((-sign * total, r))
}

/// `Mm.w.d`, `Jn` or `n`, with an optional `/time`.
fn parse_rule(s: &str) -> Option<(RuleTime, &str)> {
    let (rule, r) = if let Some(r) = s.strip_prefix('M') {
        let (m, r) = take_digits(r, 1, 2)?;
        let r = strip_one(r, '.')?;
        let (w, r) = take_digits(r, 1, 1)?;
        let r = strip_one(r, '.')?;
        let (d, r) = take_digits(r, 1, 1)?;
        if !(1..=12).contains(&m) || !(1..=5).contains(&w) || d > 6 {
            return None;
        }
        (
            Rule::Month {
                m: u8::try_from(m).ok()?,
                w: u8::try_from(w).ok()?,
                d: u8::try_from(d).ok()?,
            },
            r,
        )
    } else if let Some(r) = s.strip_prefix('J') {
        let (n, r) = take_digits(r, 1, 3)?;
        if !(1..=365).contains(&n) {
            return None;
        }
        (Rule::Julian(u16::try_from(n).ok()?), r)
    } else {
        let (n, r) = take_digits(s, 1, 3)?;
        if n > 365 {
            return None;
        }
        (Rule::ZeroDay(u16::try_from(n).ok()?), r)
    };
    let (time, r) = match r.strip_prefix('/') {
        Some(rest) => parse_rule_clock(rest)?,
        None => (2 * 3600, r),
    };
    Some((RuleTime { rule, time }, r))
}

/// A rule's `/time`: signed, and newer zic emits hours past 24.
fn parse_rule_clock(s: &str) -> Option<(i32, &str)> {
    let (sign, r) = match s.as_bytes().first() {
        Some(b'+') => (1, s.get(1..)?),
        Some(b'-') => (-1, s.get(1..)?),
        _ => (1, s),
    };
    let (h, r) = take_digits(r, 1, 3)?;
    if h > 167 {
        return None;
    }
    let (m, r) = match r.strip_prefix(':') {
        Some(rest) => take_digits(rest, 1, 2)?,
        None => (0, r),
    };
    let (sec, r) = match r.strip_prefix(':') {
        Some(rest) => take_digits(rest, 1, 2)?,
        None => (0, r),
    };
    if m > 59 || sec > 59 {
        return None;
    }
    Some((sign * i32::try_from(h * 3600 + m * 60 + sec).ok()?, r))
}

/// A POSIX TZ string such as `EST5EDT,M3.2.0,M11.1.0`.
fn parse_posix_tz(s: &str) -> Option<PosixTz> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let r = skip_tz_name(s)?;
    let (std_off, r) = parse_tz_utoff(r)?;
    let std = LocalType {
        utoff: std_off,
        isdst: false,
    };
    if r.is_empty() {
        return Some(PosixTz { std, dst: None });
    }
    let r = skip_tz_name(r)?;
    let (dst_off, r) = match parse_tz_utoff(r) {
        Some((off, rest)) => (off, rest),
        // A DST name with no offset means one hour ahead.
        None => (std_off.saturating_add(3600), r),
    };
    let ty = LocalType {
        utoff: dst_off,
        isdst: true,
    };
    // A DST name without rules leaves the transitions unspecified; keep the
    // standard offset rather than inventing them.
    let Some(r) = r.strip_prefix(',') else {
        return Some(PosixTz { std, dst: None });
    };
    let (start, r) = parse_rule(r)?;
    let r = strip_one(r, ',')?;
    let (end, r) = parse_rule(r)?;
    if !r.is_empty() {
        return None;
    }
    Some(PosixTz {
        std,
        dst: Some(Dst { ty, start, end }),
    })
}

// ------------------------------------------------------------------- TZif

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Cursor<'a> {
        Cursor { data, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let out = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(out)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1)?.first().copied()
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    fn rest(&self) -> &'a [u8] {
        self.data.get(self.pos..).unwrap_or(&[])
    }
}

struct Header {
    version: u8,
    isutcnt: u32,
    isstdcnt: u32,
    leapcnt: u32,
    timecnt: u32,
    typecnt: u32,
    charcnt: u32,
}

fn read_header(c: &mut Cursor) -> Option<Header> {
    if c.take(4)? != b"TZif" {
        return None;
    }
    let version = c.u8()?;
    if !matches!(version, 0 | b'2' | b'3' | b'4') {
        return None;
    }
    c.take(15)?;
    Some(Header {
        version,
        isutcnt: c.u32()?,
        isstdcnt: c.u32()?,
        leapcnt: c.u32()?,
        timecnt: c.u32()?,
        typecnt: c.u32()?,
        charcnt: c.u32()?,
    })
}

struct Block {
    transitions: Vec<i64>,
    indices: Vec<usize>,
    types: Vec<LocalType>,
}

fn read_block(c: &mut Cursor, h: &Header, wide: bool) -> Option<Block> {
    let timecnt = usize::try_from(h.timecnt).ok()?;
    // A version 2+ file's 32-bit block may be empty; only the block the
    // zone is built from has to carry types.
    let typecnt = usize::try_from(h.typecnt).ok()?;
    let mut transitions = Vec::new();
    for _ in 0..timecnt {
        transitions.push(if wide { c.i64()? } else { i64::from(c.i32()?) });
    }
    let mut indices = Vec::new();
    for _ in 0..timecnt {
        let idx = usize::from(c.u8()?);
        if idx >= typecnt {
            return None;
        }
        indices.push(idx);
    }
    let mut types = Vec::new();
    for _ in 0..typecnt {
        let utoff = c.i32()?;
        let isdst = c.u8()? != 0;
        let _abbr_index = c.u8()?;
        if !(-89_999..=93_599).contains(&utoff) {
            return None;
        }
        types.push(LocalType { utoff, isdst });
    }
    // Abbreviations, leap seconds and the standard/UT indicators are not
    // used here, but must be stepped over to reach the footer.
    c.take(usize::try_from(h.charcnt).ok()?)?;
    for _ in 0..h.leapcnt {
        c.take(if wide { 12 } else { 8 })?;
    }
    c.take(usize::try_from(h.isstdcnt).ok()?)?;
    c.take(usize::try_from(h.isutcnt).ok()?)?;
    let unsorted = transitions
        .windows(2)
        .any(|w| matches!((w.first(), w.get(1)), (Some(a), Some(b)) if a >= b));
    if unsorted {
        return None;
    }
    Some(Block {
        transitions,
        indices,
        types,
    })
}

fn read_footer(rest: &[u8]) -> Option<PosixTz> {
    let body = rest.strip_prefix(b"\n")?;
    let end = body.iter().position(|&b| b == b'\n')?;
    parse_posix_tz(std::str::from_utf8(body.get(..end)?).ok()?)
}

/// A time zone: the transitions a TZif file records, plus the POSIX rule
/// that governs everything after the last one.
#[derive(Debug, Clone)]
pub struct Zone {
    transitions: Vec<i64>,
    indices: Vec<usize>,
    types: Vec<LocalType>,
    initial: usize,
    footer: Option<PosixTz>,
}

impl Zone {
    /// UTC, used whenever the system zone cannot be read.
    pub fn utc() -> Zone {
        Zone {
            transitions: Vec::new(),
            indices: Vec::new(),
            types: vec![LocalType {
                utoff: 0,
                isdst: false,
            }],
            initial: 0,
            footer: None,
        }
    }

    /// The system zone, or UTC if it cannot be read or parsed.
    pub fn local() -> Zone {
        let tz = std::env::var("TZ").ok();
        let path = tz_file_path(tz.as_deref());
        std::fs::read(path)
            .ok()
            .and_then(|data| Zone::from_tzif(&data))
            .unwrap_or_else(Zone::utc)
    }

    /// Parse a TZif v1, v2 or v3 file (RFC 8536).
    pub fn from_tzif(data: &[u8]) -> Option<Zone> {
        let mut c = Cursor::new(data);
        let h1 = read_header(&mut c)?;
        let first = read_block(&mut c, &h1, false)?;
        let (block, footer) = if h1.version >= b'2' {
            // Versions 2 and up repeat everything with 64-bit times; that
            // block wins, and the footer covers the future.
            let h2 = read_header(&mut c)?;
            let wide = read_block(&mut c, &h2, true)?;
            let footer = read_footer(c.rest());
            (wide, footer)
        } else {
            (first, None)
        };
        if block.types.is_empty() {
            return None;
        }
        // Before the first transition, the first standard-time type applies.
        let initial = block.types.iter().position(|t| !t.isdst).unwrap_or(0);
        Some(Zone {
            transitions: block.transitions,
            indices: block.indices,
            types: block.types,
            initial,
            footer,
        })
    }

    /// Seconds east of UTC at an instant.
    pub fn offset_at(&self, unix: i64) -> i32 {
        let past_last = match self.transitions.last() {
            Some(&last) => unix >= last,
            None => true,
        };
        if past_last {
            if let Some(footer) = self.footer {
                return footer.type_at(unix).utoff;
            }
        }
        let idx = match self.transitions.binary_search(&unix) {
            Ok(i) => Some(i),
            Err(0) => None,
            Err(i) => Some(i - 1),
        };
        idx.and_then(|i| self.indices.get(i))
            .and_then(|&t| self.types.get(t))
            .or_else(|| self.types.get(self.initial))
            .map_or(0, |t| t.utoff)
    }

    /// Local civil time at an instant, with the offset used.
    pub fn to_local(&self, unix: i64) -> (Civil, i32) {
        let off = self.offset_at(unix);
        (unix_to_civil_utc(unix.saturating_add(i64::from(off))), off)
    }

    /// Every distinct offset the zone can produce.
    fn candidate_offsets(&self) -> Vec<i32> {
        let mut offs: Vec<i32> = self.types.iter().map(|t| t.utoff).collect();
        if let Some(footer) = self.footer {
            offs.push(footer.std.utoff);
            if let Some(dst) = footer.dst {
                offs.push(dst.ty.utoff);
            }
        }
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
mod tests {
    use super::*;

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

    // ---- TZif fixtures -------------------------------------------------

    fn push_header(out: &mut Vec<u8>, version: u8, counts: [u32; 6]) {
        out.extend_from_slice(b"TZif");
        out.push(version);
        out.extend_from_slice(&[0u8; 15]);
        for c in counts {
            out.extend_from_slice(&c.to_be_bytes());
        }
    }

    fn push_types(out: &mut Vec<u8>, types: &[(i32, bool)]) {
        for (utoff, isdst) in types {
            out.extend_from_slice(&utoff.to_be_bytes());
            out.push(u8::from(*isdst));
            out.push(0);
        }
        out.extend_from_slice(b"LMT\0");
    }

    /// A version 1 file: 32-bit transitions, no footer.
    fn tzif_v1(types: &[(i32, bool)], transitions: &[(i64, u8)]) -> Vec<u8> {
        let mut out = Vec::new();
        push_header(
            &mut out,
            0,
            [0, 0, 0, transitions.len() as u32, types.len() as u32, 4],
        );
        for (t, _) in transitions {
            out.extend_from_slice(&(*t as i32).to_be_bytes());
        }
        for (_, i) in transitions {
            out.push(*i);
        }
        push_types(&mut out, types);
        out
    }

    /// A version 2 file: a stub v1 block, then the 64-bit block and footer,
    /// which is how a modern "slim" file is laid out.
    fn tzif_v2(types: &[(i32, bool)], transitions: &[(i64, u8)], footer: &str) -> Vec<u8> {
        let mut out = Vec::new();
        push_header(&mut out, b'2', [0, 0, 0, 0, 1, 4]);
        push_types(&mut out, &[(0, false)]);
        push_header(
            &mut out,
            b'2',
            [0, 0, 0, transitions.len() as u32, types.len() as u32, 4],
        );
        for (t, _) in transitions {
            out.extend_from_slice(&t.to_be_bytes());
        }
        for (_, i) in transitions {
            out.push(*i);
        }
        push_types(&mut out, types);
        out.push(b'\n');
        out.extend_from_slice(footer.as_bytes());
        out.push(b'\n');
        out
    }

    /// US Eastern for 2024, with a footer covering everything after.
    fn eastern() -> Zone {
        let types = [(-18_000, false), (-14_400, true)];
        let transitions = [
            (unix(2024, 3, 10, 7, 0, 0), 1u8),
            (unix(2024, 11, 3, 6, 0, 0), 0u8),
        ];
        Zone::from_tzif(&tzif_v2(&types, &transitions, "EST5EDT,M3.2.0,M11.1.0"))
            .expect("fixture parses")
    }

    #[test]
    fn tzif_v2_to_local_before_between_and_after_transitions() {
        let z = eastern();
        // Before the first transition the initial standard type applies.
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
    fn tzif_v2_from_local_at_a_gap_and_an_overlap() {
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
    fn tzif_v1_without_a_footer_holds_the_last_type() {
        let types = [(-18_000, false), (-14_400, true)];
        let transitions = [
            (unix(2024, 3, 10, 7, 0, 0), 1u8),
            (unix(2024, 11, 3, 6, 0, 0), 0u8),
        ];
        let z = Zone::from_tzif(&tzif_v1(&types, &transitions)).expect("v1 parses");
        assert_eq!(z.offset_at(unix(2024, 1, 1, 0, 0, 0)), -18_000);
        assert_eq!(z.offset_at(unix(2024, 6, 1, 0, 0, 0)), -14_400);
        // No footer: the last recorded type simply continues.
        assert_eq!(z.offset_at(unix(2025, 7, 1, 0, 0, 0)), -18_000);
    }

    #[test]
    fn a_footer_only_zone_needs_no_transitions() {
        let z = Zone::from_tzif(&tzif_v2(&[(0, false)], &[], "<+0330>-3:30")).expect("parses");
        assert_eq!(z.offset_at(unix(2024, 1, 1, 0, 0, 0)), 12_600);
        assert_eq!(
            z.to_local(unix(2024, 1, 1, 0, 0, 0)),
            (civil(2024, 1, 1, 3, 30, 0), 12_600)
        );
    }

    #[test]
    fn an_empty_32_bit_block_is_tolerated_but_an_empty_64_bit_one_is_not() {
        // RFC 8536 lets a version 2+ file leave its 32-bit block empty.
        let mut file = Vec::new();
        push_header(&mut file, b'2', [0, 0, 0, 0, 0, 0]);
        push_header(&mut file, b'2', [0, 0, 0, 0, 1, 4]);
        push_types(&mut file, &[(-18_000, false)]);
        file.push(b'\n');
        file.extend_from_slice(b"EST5EDT,M3.2.0,M11.1.0");
        file.push(b'\n');
        let z = Zone::from_tzif(&file).expect("empty v1 block parses");
        assert_eq!(z.offset_at(unix(2024, 7, 1, 12, 0, 0)), -14_400);

        let mut empty = Vec::new();
        push_header(&mut empty, b'2', [0, 0, 0, 0, 0, 0]);
        push_header(&mut empty, b'2', [0, 0, 0, 0, 0, 0]);
        empty.extend_from_slice(b"\nEST5\n");
        assert!(Zone::from_tzif(&empty).is_none());
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
        let z = Zone::from_tzif(&tzif_v2(
            &[(43_200, false)],
            &[],
            "NZST-12NZDT,M9.5.0,M4.1.0/3",
        ))
        .expect("parses");
        assert_eq!(z.offset_at(unix(2024, 1, 15, 0, 0, 0)), 46_800);
        assert_eq!(z.offset_at(unix(2024, 6, 15, 0, 0, 0)), 43_200);
        assert_eq!(z.offset_at(unix(2024, 12, 15, 0, 0, 0)), 46_800);
    }

    #[test]
    fn malformed_tzif_is_rejected_and_utc_stands_in() {
        for bad in [
            &b""[..],
            b"TZif",
            b"nope\0garbage",
            &tzif_v2(&[(0, false)], &[], "EST5EDT,M3.2.0,M11.1.0")[..4],
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

    // ---- POSIX TZ footer ------------------------------------------------

    #[test]
    fn footer_corpus_from_real_zones() {
        let est = parse_posix_tz("EST5EDT,M3.2.0,M11.1.0").expect("EST5EDT");
        assert_eq!(est.std.utoff, -18_000);
        let dst = est.dst.expect("has dst");
        assert_eq!(dst.ty.utoff, -14_400);
        assert_eq!(
            dst.start,
            RuleTime {
                rule: Rule::Month { m: 3, w: 2, d: 0 },
                time: 7200
            }
        );
        assert_eq!(
            dst.end,
            RuleTime {
                rule: Rule::Month { m: 11, w: 1, d: 0 },
                time: 7200
            }
        );

        let cet = parse_posix_tz("CET-1CEST,M3.5.0,M10.5.0/3").expect("CET");
        assert_eq!(cet.std.utoff, 3600);
        let dst = cet.dst.expect("has dst");
        assert_eq!(dst.ty.utoff, 7200);
        assert_eq!(dst.end.time, 10_800);

        let iran = parse_posix_tz("<+0330>-3:30").expect("+0330");
        assert_eq!(iran.std.utoff, 12_600);
        assert!(iran.dst.is_none());

        let nz = parse_posix_tz("NZST-12NZDT,M9.5.0,M4.1.0/3").expect("NZ");
        assert_eq!(nz.std.utoff, 43_200);
        assert_eq!(nz.dst.map(|d| d.ty.utoff), Some(46_800));

        let west = parse_posix_tz("<-03>3").expect("-03");
        assert_eq!(west.std.utoff, -10_800);
        assert!(west.dst.is_none());

        // Ireland: the DST offset is behind standard time.
        let eire = parse_posix_tz("IST-1GMT0,M10.5.0,M3.5.0/1").expect("Eire");
        assert_eq!(eire.std.utoff, 3600);
        assert_eq!(eire.dst.map(|d| d.ty.utoff), Some(0));

        // Nuuk: a negative rule time.
        let nuuk = parse_posix_tz("<-02>2<-01>,M3.5.0/-1,M10.5.0/0").expect("Nuuk");
        assert_eq!(nuuk.std.utoff, -7200);
        let dst = nuuk.dst.expect("has dst");
        assert_eq!(dst.ty.utoff, -3600);
        assert_eq!(dst.start.time, -3600);
        assert_eq!(dst.end.time, 0);

        // Hours past 24, as newer zic emits.
        let late = parse_posix_tz("EST5EDT,M3.2.0/26:00,M11.1.0").expect("late rule");
        assert_eq!(late.dst.map(|d| d.start.time), Some(93_600));

        // A DST name with no offset means one hour ahead.
        let implied = parse_posix_tz("AEST-10AEDT,M10.1.0,M4.1.0/3").expect("AEST");
        assert_eq!(implied.std.utoff, 36_000);
        assert_eq!(implied.dst.map(|d| d.ty.utoff), Some(39_600));

        // Day-of-year rules.
        let julian = parse_posix_tz("XXX0YYY,J60,J300").expect("julian");
        assert_eq!(julian.dst.map(|d| d.start.rule), Some(Rule::Julian(60)));
        let zero = parse_posix_tz("XXX0YYY,59,300").expect("zero based");
        assert_eq!(zero.dst.map(|d| d.start.rule), Some(Rule::ZeroDay(59)));
    }

    #[test]
    fn footer_rejects_nonsense() {
        for bad in [
            "",
            "5",
            "EST",
            "EST5EDT,M13.2.0,M11.1.0",
            "EST5EDT,M3.6.0,M11.1.0",
            "EST5EDT,M3.2.7,M11.1.0",
            "EST5EDT,J0,M11.1.0",
            "EST5EDT,J366,M11.1.0",
            "EST5EDT,M3.2.0",
            "EST5EDT,M3.2.0,M11.1.0,junk",
        ] {
            assert_eq!(parse_posix_tz(bad), None, "should reject {:?}", bad);
        }
    }

    #[test]
    fn rule_days_land_on_the_right_dates() {
        let second_sunday_march = rule_day(2024, Rule::Month { m: 3, w: 2, d: 0 });
        assert_eq!(
            unix_to_civil_utc(second_sunday_march * SECS_PER_DAY).day,
            10
        );
        let first_sunday_november = rule_day(2024, Rule::Month { m: 11, w: 1, d: 0 });
        assert_eq!(
            unix_to_civil_utc(first_sunday_november * SECS_PER_DAY),
            civil(2024, 11, 3, 0, 0, 0)
        );
        // Week 5 means the last such weekday in the month.
        assert_eq!(
            unix_to_civil_utc(rule_day(2024, Rule::Month { m: 3, w: 5, d: 0 }) * SECS_PER_DAY),
            civil(2024, 3, 31, 0, 0, 0)
        );
        assert_eq!(
            unix_to_civil_utc(rule_day(2023, Rule::Month { m: 10, w: 5, d: 0 }) * SECS_PER_DAY),
            civil(2023, 10, 29, 0, 0, 0)
        );
        // Jn skips February 29; n counts it.
        assert_eq!(
            unix_to_civil_utc(rule_day(2024, Rule::Julian(60)) * SECS_PER_DAY),
            civil(2024, 3, 1, 0, 0, 0)
        );
        assert_eq!(
            unix_to_civil_utc(rule_day(2023, Rule::Julian(60)) * SECS_PER_DAY),
            civil(2023, 3, 1, 0, 0, 0)
        );
        assert_eq!(
            unix_to_civil_utc(rule_day(2024, Rule::ZeroDay(59)) * SECS_PER_DAY),
            civil(2024, 2, 29, 0, 0, 0)
        );
    }
}
