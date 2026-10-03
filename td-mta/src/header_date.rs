//! Complete resident RFC 5322 date-time parsing; output formatting is separate.
pub mod project;

use crate::{
    admission::work::{Charge, Meter, Stop},
    header_cfws,
    ports::Tick,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Offset {
    /// Minutes east of UTC, including RFC 5322 offsets beyond 23 hours.
    Known(i16),
    /// -0000 or an obsolete zone without a known local offset.
    Unknown,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Date {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub offset: Offset,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete(Option<Date>),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    NestingLimit,
    Work(Stop),
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NestingLimit => f.write_str("date comment nesting limit"),
            Self::Work(error) => write!(f, "date work: {error}"),
            Self::InvalidState => f.write_str("invalid date parser state"),
        }
    }
}
impl std::error::Error for Error {}
impl From<Stop> for Error {
    fn from(error: Stop) -> Self {
        Self::Work(error)
    }
}

#[derive(Clone, Copy, Default)]
struct Number {
    value: u32,
    digits: usize,
}
#[derive(Clone, Copy)]
enum Token {
    Number(Number),
    Word { prefix: [u8; 3], len: u8 },
    Byte(u8),
    End,
}
#[derive(Clone, Copy)]
enum Phase {
    Start,
    Comma,
    Day,
    Month,
    Year,
    YearEnd,
    Colon,
    Minute,
    SecondOrZone,
    Second,
    Zone,
    Offset,
    End,
}
/// The borrow must contain the complete field value, excluding its final CRLF.
pub struct Cursor<'a> {
    source: &'a [u8],
    position: usize,
    cfws: Option<header_cfws::Cursor<'a>>,
    token: Option<Token>,
    gap: bool,
    trailing_wsp: bool,
    phase: Phase,
    year: Number,
    weekday: Option<u8>,
    negative: bool,
    date: Date,
    complete: Option<Option<Date>>,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    pub const fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            position: 0,
            cfws: Some(header_cfws::Cursor::new(source, 0)),
            token: None,
            gap: false,
            trailing_wsp: false,
            phase: Phase::Start,
            year: Number {
                value: 0,
                digits: 0,
            },
            weekday: None,
            negative: false,
            date: Date {
                year: 0,
                month: 0,
                day: 0,
                hour: 0,
                minute: 0,
                second: 0,
                offset: Offset::Unknown,
            },
            complete: None,
            failure: None,
        }
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if let Some(value) = self.complete {
            return Ok(Status::Complete(value));
        }
        let result = self.step(now, work);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn finish(&mut self, value: Option<Date>) -> Status {
        self.complete = Some(value);
        Status::Complete(value)
    }
    fn step(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        if let Some(cursor) = self.cfws.as_mut() {
            match cursor.poll(now, work) {
                Ok(header_cfws::Status::Comment(_) | header_cfws::Status::Yield) => {}
                Ok(header_cfws::Status::Complete(end)) => {
                    self.position = end.position;
                    self.gap = end.consumed;
                    // This extra source visit distinguishes FWS before numeric zones.
                    charge(now, work, u64::from(end.consumed), 1)?;
                    self.trailing_wsp = if end.consumed {
                        let previous = end.position.checked_sub(1).ok_or(Error::InvalidState)?;
                        matches!(self.source.get(previous), Some(b' ' | b'\t'))
                    } else {
                        false
                    };
                    self.cfws = None;
                }
                Err(header_cfws::Error::Malformed) => return Ok(self.finish(None)),
                Err(header_cfws::Error::NestingLimit) => return Err(Error::NestingLimit),
                Err(header_cfws::Error::Work(stop)) => return Err(Error::Work(stop)),
                Err(header_cfws::Error::InvalidState) => return Err(Error::InvalidState),
            }
            return Ok(Status::Yield);
        }
        for _ in 0..32 {
            charge(now, work, u64::from(self.position < self.source.len()), 1)?;
            let byte = self.source.get(self.position).copied();
            let next = match (self.token, byte) {
                (None, Some(b'0'..=b'9')) => Some(Token::Number(Number::default())),
                (None, Some(b'a'..=b'z' | b'A'..=b'Z')) => Some(Token::Word {
                    prefix: [0; 3],
                    len: 0,
                }),
                (token, _) => token,
            };
            let continuing = match (next, byte) {
                (Some(Token::Number(mut number)), Some(byte @ b'0'..=b'9')) => {
                    let Some(value) = number
                        .value
                        .checked_mul(10)
                        .and_then(|v| v.checked_add(u32::from(byte - b'0')))
                    else {
                        return Ok(self.finish(None));
                    };
                    number.value = value;
                    number.digits = number.digits.checked_add(1).ok_or(Error::InvalidState)?;
                    Some(Token::Number(number))
                }
                (
                    Some(Token::Word { mut prefix, len }),
                    Some(byte @ (b'a'..=b'z' | b'A'..=b'Z')),
                ) => {
                    if let Some(slot) = prefix.get_mut(usize::from(len)) {
                        *slot = byte.to_ascii_lowercase();
                    }
                    Some(Token::Word {
                        prefix,
                        len: len.saturating_add(1).min(4),
                    })
                }
                _ => None,
            };
            if let Some(token) = continuing {
                self.token = Some(token);
                self.position = self.position.checked_add(1).ok_or(Error::InvalidState)?;
                continue;
            }
            let token = if let Some(token) = self.token.take() {
                token
            } else if let Some(byte) = byte {
                self.position = self.position.checked_add(1).ok_or(Error::InvalidState)?;
                Token::Byte(byte)
            } else {
                Token::End
            };
            if !self.accept(token) {
                return Ok(self.finish(None));
            }
            if matches!(token, Token::End) {
                return Ok(self.finish(self.valid().then_some(self.date)));
            }
            self.cfws = Some(header_cfws::Cursor::new(self.source, self.position));
            return Ok(Status::Yield);
        }
        Ok(Status::Yield)
    }
    fn accept(&mut self, token: Token) -> bool {
        match (self.phase, token) {
            (Phase::Start, Token::Word { prefix, len: 3 }) => {
                let Some(day) = [
                    *b"mon", *b"tue", *b"wed", *b"thu", *b"fri", *b"sat", *b"sun",
                ]
                .iter()
                .position(|name| *name == prefix) else {
                    return false;
                };
                self.weekday = u8::try_from(day).ok();
                self.phase = Phase::Comma;
            }
            (Phase::Comma, Token::Byte(b',')) => self.phase = Phase::Day,
            (Phase::Start | Phase::Day, Token::Number(number))
                if (1..=2).contains(&number.digits) && (1..=31).contains(&number.value) =>
            {
                self.date.day = number.value as u8;
                self.phase = Phase::Month;
            }
            (Phase::Month, Token::Word { prefix, len: 3 }) => {
                let Some(month) = [
                    *b"jan", *b"feb", *b"mar", *b"apr", *b"may", *b"jun", *b"jul", *b"aug",
                    *b"sep", *b"oct", *b"nov", *b"dec",
                ]
                .iter()
                .position(|name| *name == prefix) else {
                    return false;
                };
                let Ok(month) = u8::try_from(month + 1) else {
                    return false;
                };
                self.date.month = month;
                self.phase = Phase::Year;
            }
            (Phase::Year, Token::Number(number)) => {
                self.year = number;
                self.phase = Phase::YearEnd;
            }
            (Phase::YearEnd, Token::Number(number)) => {
                if !self.set_year(self.year) || !clock(number, 23, &mut self.date.hour) {
                    return false;
                }
                self.phase = Phase::Colon;
            }
            (Phase::YearEnd, Token::Byte(b':')) => {
                // obs-year and obs-hour can be adjacent; the last two digits are hour.
                let Some(digits) = self.year.digits.checked_sub(2) else {
                    return false;
                };
                if !self.set_year(Number {
                    value: self.year.value / 100,
                    digits,
                }) || !clock(
                    Number {
                        value: self.year.value % 100,
                        digits: 2,
                    },
                    23,
                    &mut self.date.hour,
                ) {
                    return false;
                }
                self.phase = Phase::Minute;
            }
            (Phase::Colon, Token::Byte(b':')) => self.phase = Phase::Minute,
            (Phase::Minute, Token::Number(number)) => {
                if !clock(number, 59, &mut self.date.minute) {
                    return false;
                }
                self.phase = Phase::SecondOrZone;
            }
            (Phase::SecondOrZone, Token::Byte(b':')) => self.phase = Phase::Second,
            (Phase::Second, Token::Number(number)) => {
                if !clock(number, 60, &mut self.date.second) {
                    return false;
                }
                self.phase = Phase::Zone;
            }
            (Phase::SecondOrZone | Phase::Zone, Token::Byte(sign @ (b'+' | b'-')))
                if self.trailing_wsp =>
            {
                self.negative = sign == b'-';
                self.phase = Phase::Offset;
            }
            (Phase::SecondOrZone | Phase::Zone, Token::Word { prefix, len }) => {
                let Some(offset) = obsolete_zone(prefix, len) else {
                    return false;
                };
                self.date.offset = offset;
                self.phase = Phase::End;
            }
            (Phase::Offset, Token::Number(number))
                if !self.gap && number.digits == 4 && number.value % 100 < 60 =>
            {
                // Exactly four digits bounds the value to 9959, or 5999 minutes.
                let minutes = (number.value / 100 * 60 + number.value % 100) as i16;
                self.date.offset = if self.negative && minutes == 0 {
                    Offset::Unknown
                } else {
                    Offset::Known(if self.negative { -minutes } else { minutes })
                };
                self.phase = Phase::End;
            }
            (Phase::End, Token::End) => {}
            _ => return false,
        }
        true
    }
    fn set_year(&mut self, number: Number) -> bool {
        let year = match number.digits {
            2 => number.value + if number.value < 50 { 2000 } else { 1900 },
            3 => number.value + 1900,
            4.. => number.value,
            _ => return false,
        };
        if !(1900..=9999).contains(&year) {
            return false;
        }
        self.date.year = year as u16;
        true
    }
    fn valid(&self) -> bool {
        let Date {
            year, month, day, ..
        } = self.date;
        if day > month_days(year, month) {
            return false;
        }
        let prior = u32::from(year) - 1;
        let mut days = 365 * prior + prior / 4 - prior / 100 + prior / 400;
        for earlier in 1..month {
            days += u32::from(month_days(year, earlier));
        }
        days += u32::from(day) - 1;
        self.weekday
            .is_none_or(|weekday| u32::from(weekday) == days % 7)
    }
}
fn clock(number: Number, max: u32, destination: &mut u8) -> bool {
    if number.digits != 2 || number.value > max {
        return false;
    }
    *destination = number.value as u8;
    true
}
fn month_days(year: u16, month: u8) -> u8 {
    match month {
        2 if year.is_multiple_of(400) || (year.is_multiple_of(4) && !year.is_multiple_of(100)) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => 0,
    }
}
fn obsolete_zone(prefix: [u8; 3], len: u8) -> Option<Offset> {
    let known = match (len, &prefix) {
        (2, b"ut\0") | (3, b"gmt") => Some(0),
        (3, b"est") => Some(-300),
        (3, b"edt") => Some(-240),
        (3, b"cst") => Some(-360),
        (3, b"cdt") => Some(-300),
        (3, b"mst") => Some(-420),
        (3, b"mdt") => Some(-360),
        (3, b"pst") => Some(-480),
        (3, b"pdt") => Some(-420),
        _ => None,
    };
    if let Some(minutes) = known {
        return Some(Offset::Known(minutes));
    }
    match (len, prefix.first().copied()) {
        (1, Some(b'a'..=b'i' | b'k'..=b'z')) | (2.., _) => Some(Offset::Unknown),
        _ => None,
    }
}
fn charge(now: Tick, work: &mut Meter, bytes: u64, records: u64) -> Result<(), Error> {
    work.charge(
        now,
        Charge {
            io_bytes: bytes,
            records,
            ..Charge::default()
        },
    )?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::ports::Deadline;
    fn work() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 10_000_000,
                records: 2_000_000,
                ..Charge::default()
            },
        )
    }
    fn parse(source: &[u8]) -> Result<Option<Date>, Error> {
        let mut cursor = Cursor::new(source);
        assert!(std::mem::size_of_val(&cursor) <= 192);
        let mut work = work();
        for _ in 0..100_000 {
            let before = work.remaining();
            let status = cursor.poll(Tick(1), &mut work)?;
            let after = work.remaining();
            assert!(before.io_bytes - after.io_bytes <= 161);
            assert!(before.records - after.records <= 33);
            assert_eq!(before.output_bytes, after.output_bytes);
            if let Status::Complete(value) = status {
                assert_eq!(cursor.poll(Tick(100), &mut work), Ok(status));
                assert_eq!(after, work.remaining());
                return Ok(value);
            }
        }
        panic!("date did not terminate");
    }
    #[test]
    fn complete_dates_obsolete_spacing_and_years() {
        let expected = Date {
            year: 1997,
            month: 11,
            day: 21,
            hour: 9,
            minute: 55,
            second: 6,
            offset: Offset::Known(-360),
        };
        for source in [
            b"Fri, 21 Nov 1997 09:55:06 -0600".as_slice(),
            b"21 nov 97 09:55:06 CST",
            b"FRI(comment),(a)21(b)NOV(c)097(d)09(e):(f)55(g):(h)06(i) CST(j)",
            b"21Nov199709:55:06CST",
            b"21Nov9709:55:06cst",
            b"21Nov09709:55:06 CST",
            b"21 Nov 199709 (c) :55:06 CST",
            b"21\r\n Nov\n\t1997 09:55:06 -0600",
            "(🐈)21 Nov 1997 09:55:06 -0600(é)".as_bytes(),
        ] {
            assert_eq!(parse(source), Ok(Some(expected)), "{source:?}");
        }
        for (year, expected) in [
            ("00", 2000),
            ("49", 2049),
            ("50", 1950),
            ("99", 1999),
            ("000", 1900),
            ("049", 1949),
            ("999", 2899),
            ("1900", 1900),
            ("9999", 9999),
            ("00002024", 2024),
        ] {
            let source = format!("1 Jan {year} 00:00 +0000");
            let value = parse(source.as_bytes()).unwrap().unwrap();
            assert_eq!(value.year, expected);
            assert_eq!(value.second, 0);
        }
    }
    #[test]
    fn numeric_and_obsolete_zones_keep_unknown_distinct() {
        for (zone, offset) in [
            ("+0000", Offset::Known(0)),
            ("-0000", Offset::Unknown),
            ("+0530", Offset::Known(330)),
            ("-0330", Offset::Known(-210)),
            ("+9959", Offset::Known(5999)),
            ("-9959", Offset::Known(-5999)),
            ("UT", Offset::Known(0)),
            ("gmt", Offset::Known(0)),
            ("EST", Offset::Known(-300)),
            ("EDT", Offset::Known(-240)),
            ("CST", Offset::Known(-360)),
            ("CDT", Offset::Known(-300)),
            ("MST", Offset::Known(-420)),
            ("MDT", Offset::Known(-360)),
            ("PST", Offset::Known(-480)),
            ("PDT", Offset::Known(-420)),
            ("A", Offset::Unknown),
            ("i", Offset::Unknown),
            ("K", Offset::Unknown),
            ("z", Offset::Unknown),
            ("JST", Offset::Unknown),
            ("unknownzone", Offset::Unknown),
        ] {
            let source = format!("1 Jan 2000 00:00 {zone}");
            assert_eq!(
                parse(source.as_bytes()).unwrap().unwrap().offset,
                offset,
                "{zone}"
            );
        }
        assert!(parse(b"1 Jan 2000 00:00GMT").unwrap().is_some());
        assert!(parse(b"1 Jan 2000 00:00 (x) +0000").unwrap().is_some());
        for source in [
            b"1 Jan 2000 00:00+0000".as_slice(),
            b"1 Jan 2000 00:00 (x)+0000",
            b"1 Jan 2000 00:00 + 0000",
            b"1 Jan 2000 00:00 +(x)0000",
            b"1 Jan 2000 00:00 J",
            b"1 Jan 2000 00:00 +0060",
            b"1 Jan 2000 00:00 +00000",
        ] {
            assert_eq!(parse(source), Ok(None), "{source:?}");
        }
    }
    #[test]
    fn weekday_calendar_clock_and_complete_field_validation() {
        for source in [
            b"Mon, 1 Jan 1900 00:00 +0000".as_slice(),
            b"Tue, 29 Feb 2000 23:59:60 +0000",
            b"Thu, 29 Feb 2024 23:59:59 +0000",
            b"Fri, 31 Dec 9999 23:59:59 -0000",
        ] {
            assert!(parse(source).unwrap().is_some(), "{source:?}");
        }
        for source in [
            b"".as_slice(),
            b"(comment)",
            b"Sun, 1 Jan 1900 00:00 +0000",
            b"29 Feb 1900 00:00 +0000",
            b"29 Feb 2100 00:00 +0000",
            b"31 Apr 2000 00:00 +0000",
            b"0 Jan 2000 00:00 +0000",
            b"001 Jan 2000 00:00 +0000",
            b"1 January 2000 00:00 +0000",
            b"1 Jan 1899 00:00 +0000",
            b"1 Jan 10000 00:00 +0000",
            b"1 Jan 0 00:00 +0000",
            b"1 Jan 2000 24:00 +0000",
            b"1 Jan 2000 00:60 +0000",
            b"1 Jan 2000 00:00:61 +0000",
            b"1 Jan 2000 0:00 +0000",
            b"1 Jan 2000 00:0 +0000",
            b"1 Jan 2000 00:00:0 +0000",
            b"1 Jan 2000 00:00",
            b"1 Jan 2000 00:00 +0000 junk",
            b"1 Jan 2000 00:00 +0000\r\n",
            b"1 Jan 2000 00:00 +0000(unclosed",
            b"1 Jan 2000 00:00 +0000(\xff)",
            b"Fri 21 Nov 1997 09:55:06 -0600",
            b"Fri,,21 Nov 1997 09:55:06 -0600",
        ] {
            assert_eq!(parse(source), Ok(None), "{source:?}");
        }
    }
    #[test]
    fn long_comments_words_and_zero_prefixed_years_stay_bounded() {
        let comment = format!("({}) 1 Jan 2000 00:00 +0000", "🐈".repeat(10_000));
        assert_eq!(parse(comment.as_bytes()).unwrap().unwrap().year, 2000);
        let year = format!("1 Jan {}2024 00:00 +0000", "0".repeat(10_000));
        assert_eq!(parse(year.as_bytes()).unwrap().unwrap().year, 2024);
        let zone = format!("1 Jan 2000 00:00 {}", "x".repeat(10_000));
        assert_eq!(
            parse(zone.as_bytes()).unwrap().unwrap().offset,
            Offset::Unknown
        );
        assert_eq!(parse(b"1 Jan 999999999999999999999 00:00 +0000"), Ok(None));
        let nested = format!("{}{}1 Jan 2000 00:00 +0000", "(".repeat(33), ")".repeat(33));
        assert_eq!(parse(nested.as_bytes()), Err(Error::NestingLimit));
    }
    #[test]
    fn exact_own_charges_and_refusals_survive_replacement_meters() {
        for (source, bytes, records) in [
            (b"1 Jan 2000 00:00 +0000".as_slice(), 39, 47),
            (b"1 Jan 2000 00:00 (c) +0000".as_slice(), 43, 51),
            (b"".as_slice(), 0, 3),
            (b" ".as_slice(), 2, 4),
        ] {
            let mut cursor = Cursor::new(source);
            let mut work = work();
            let before = work.remaining();
            let mut complete = false;
            for _ in 0..100 {
                if let Status::Complete(value) = cursor.poll(Tick(1), &mut work).unwrap() {
                    assert_eq!(value.is_some(), source.len() > 1);
                    complete = true;
                    break;
                }
            }
            assert!(complete);
            assert_eq!(
                before.io_bytes - work.remaining().io_bytes,
                bytes,
                "{source:?}"
            );
            assert_eq!(
                before.records - work.remaining().records,
                records,
                "{source:?}"
            );
        }
        // Empty initial CFWS completes; the next poll's token charge refuses.
        for (io_bytes, records, expected) in [(1, 100, Stop::IoBytes), (100, 2, Stop::Records)] {
            let mut cursor = Cursor::new(b"1 Jan 2000 00:00 +0000");
            let mut limited = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes,
                    records,
                    ..Charge::default()
                },
            );
            assert_eq!(cursor.poll(Tick(1), &mut limited), Ok(Status::Yield));
            assert!(cursor.cfws.is_none());
            let before = limited.remaining();
            assert_eq!(
                cursor.poll(Tick(1), &mut limited),
                Err(Error::Work(expected))
            );
            assert_eq!(limited.remaining(), before);
            assert_eq!(
                cursor.poll(Tick(1), &mut work()),
                Err(Error::Work(expected))
            );
        }
        // The child completes a nonempty run before the parent's FWS charge.
        for (io_bytes, records, expected) in [(2, 100, Stop::IoBytes), (100, 2, Stop::Records)] {
            let mut cursor = Cursor::new(b" 1 Jan 2000 00:00 +0000");
            let mut limited = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes,
                    records,
                    ..Charge::default()
                },
            );
            assert_eq!(
                cursor.poll(Tick(1), &mut limited),
                Err(Error::Work(expected))
            );
            assert_eq!(
                cursor.cfws.as_mut().unwrap().poll(Tick(1), &mut work()),
                Ok(header_cfws::Status::Complete(header_cfws::End {
                    position: 1,
                    consumed: true
                }))
            );
            assert_eq!(
                cursor.poll(Tick(1), &mut work()),
                Err(Error::Work(expected))
            );
        }
    }
    #[test]
    fn limits_and_deadlines_never_become_null_or_restart() {
        for (capacity, expected) in [
            (
                Charge {
                    records: 100,
                    ..Charge::default()
                },
                Stop::IoBytes,
            ),
            (
                Charge {
                    io_bytes: 100,
                    ..Charge::default()
                },
                Stop::Records,
            ),
        ] {
            let mut cursor = Cursor::new(b"1 Jan 2000 00:00 +0000");
            let mut limited = Meter::new(Deadline::after(Tick(0), 100).unwrap(), capacity);
            assert_eq!(
                cursor.poll(Tick(1), &mut limited),
                Err(Error::Work(expected))
            );
            assert_eq!(
                cursor.poll(Tick(1), &mut work()),
                Err(Error::Work(expected))
            );
        }
        let source = format!("({})1 Jan 2000 00:00 +0000", "x".repeat(1000));
        let mut cursor = Cursor::new(source.as_bytes());
        assert_eq!(cursor.poll(Tick(1), &mut work()), Ok(Status::Yield));
        assert_eq!(
            cursor.poll(Tick(100), &mut work()),
            Err(Error::Work(Stop::Deadline))
        );
        assert_eq!(
            cursor.poll(Tick(1), &mut work()),
            Err(Error::Work(Stop::Deadline))
        );
        let source = format!("{}{}1 Jan 2000 00:00 +0000", "(".repeat(33), ")".repeat(33));
        let mut cursor = Cursor::new(source.as_bytes());
        assert_eq!(cursor.poll(Tick(1), &mut work()), Ok(Status::Yield));
        assert_eq!(cursor.poll(Tick(1), &mut work()), Err(Error::NestingLimit));
        assert_eq!(cursor.poll(Tick(1), &mut work()), Err(Error::NestingLimit));
    }
}
