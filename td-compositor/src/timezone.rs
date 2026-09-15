//! Bounded TZif v2/v3 rules for the status clock (RFC 9636).
//! No environment, filesystem, process-global timezone, or leap-second scale.

pub const MAX_BYTES: usize = 65_536;
const DAY: i64 = 86_400;
const MAX_DESIGNATION: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Kind {
    offset: i32,
    daylight: bool,
    name: Vec<u8>,
}

#[derive(Debug)]
pub struct Zone {
    transitions: Vec<(i64, usize)>,
    kinds: Vec<Kind>,
    future: Option<Future>,
}

impl Zone {
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_BYTES {
            return None;
        }
        let mut cursor = Cursor(bytes);
        let first = Header::read(&mut cursor)?;
        cursor.take(first.size(4)?)?;
        let header = Header::read(&mut cursor)?;
        if first.version != header.version || header.leaps != 0 {
            return None;
        }
        let mut block = Cursor(cursor.take(header.size(8)?)?);
        let mut times = Vec::with_capacity(header.times);
        for _ in 0..header.times {
            let at = i64::from_be_bytes(block.take(8)?.try_into().ok()?);
            if times.last().is_some_and(|last| *last >= at) {
                return None;
            }
            times.push(at);
        }
        let indices = block.take(header.times)?;
        let records = block.take(header.kinds.checked_mul(6)?)?;
        let names = block.take(header.names)?;
        if names.last() != Some(&0) {
            return None;
        }
        let mut kinds = Vec::with_capacity(header.kinds);
        for record in records.as_chunks::<6>().0 {
            let offset = i32::from_be_bytes(record.get(..4)?.try_into().ok()?);
            let daylight = match record.get(4)? {
                0 => false,
                1 => true,
                _ => return None,
            };
            if !(-89_999..=93_599).contains(&offset) {
                return None;
            }
            let suffix = names.get(usize::from(*record.get(5)?)..)?;
            let end = suffix.iter().position(|byte| *byte == 0)?;
            if end > MAX_DESIGNATION {
                return None;
            }
            kinds.push(Kind {
                offset,
                daylight,
                name: suffix.get(..end)?.to_vec(),
            });
        }
        let standard = block.take(header.standard)?;
        let universal = block.take(header.universal)?;
        if standard.iter().any(|byte| *byte > 1)
            || universal
                .iter()
                .enumerate()
                .any(|(index, byte)| *byte > 1 || (*byte == 1 && standard.get(index) != Some(&1)))
        {
            return None;
        }
        let mut transitions = Vec::with_capacity(header.times);
        for (at, index) in times.into_iter().zip(indices) {
            let index = usize::from(*index);
            kinds.get(index)?;
            transitions.push((at, index));
        }
        let footer = cursor.0.strip_prefix(b"\n")?.strip_suffix(b"\n")?;
        let future = if footer.is_empty() {
            None
        } else {
            Some(Future::parse(footer, header.version)?)
        };
        if let (Some(future), Some((at, index))) = (&future, transitions.last()) {
            if future.kind_at(*at)? != kinds.get(*index)? {
                return None;
            }
        }
        Some(Self {
            transitions,
            kinds,
            future,
        })
    }

    /// Unknown time ranges and RFC's `-00` placeholder have no offset.
    pub fn offset_at(&self, at: i64) -> Option<i32> {
        let kind = if self.transitions.last().is_none_or(|(last, _)| at >= *last) {
            match &self.future {
                Some(future) => future.kind_at(at)?,
                None if self.transitions.is_empty() => self.kinds.first()?,
                None => return None,
            }
        } else {
            let count = self.transitions.partition_point(|(time, _)| *time <= at);
            match count.checked_sub(1) {
                Some(index) => self.kinds.get(self.transitions.get(index)?.1)?,
                None => self.kinds.first()?,
            }
        };
        (kind.name != b"-00").then_some(kind.offset)
    }
}

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(count)?;
        self.0 = tail;
        Some(head)
    }

    fn count(&mut self) -> Option<usize> {
        usize::try_from(u32::from_be_bytes(self.take(4)?.try_into().ok()?)).ok()
    }
}

struct Header {
    version: u8,
    universal: usize,
    standard: usize,
    leaps: usize,
    times: usize,
    kinds: usize,
    names: usize,
}

impl Header {
    fn read(cursor: &mut Cursor<'_>) -> Option<Self> {
        if cursor.take(4)? != b"TZif" {
            return None;
        }
        let version = *cursor.take(1)?.first()?;
        if !matches!(version, b'2' | b'3') {
            return None;
        }
        cursor.take(15)?;
        let header = Self {
            version,
            universal: cursor.count()?,
            standard: cursor.count()?,
            leaps: cursor.count()?,
            times: cursor.count()?,
            kinds: cursor.count()?,
            names: cursor.count()?,
        };
        if !(1..=256).contains(&header.kinds)
            || !(1..=MAX_BYTES).contains(&header.names)
            || header.times > MAX_BYTES / 9
            || !matches!(header.standard, 0) && header.standard != header.kinds
            || !matches!(header.universal, 0) && header.universal != header.kinds
        {
            return None;
        }
        Some(header)
    }

    fn size(&self, width: usize) -> Option<usize> {
        self.times
            .checked_mul(width + 1)?
            .checked_add(self.kinds.checked_mul(6)?)?
            .checked_add(self.names)?
            .checked_add(self.leaps.checked_mul(width + 4)?)?
            .checked_add(self.standard)?
            .checked_add(self.universal)
    }
}

#[derive(Debug)]
struct Future {
    standard: Kind,
    daylight: Option<(Kind, Rule, Rule)>,
}

#[derive(Debug, Clone, Copy)]
enum Date {
    Julian(u16),
    Ordinal(u16),
    Month { month: u8, week: u8, weekday: u8 },
}

#[derive(Debug, Clone, Copy)]
struct Rule {
    date: Date,
    seconds: i32,
}

impl Future {
    fn parse(bytes: &[u8], version: u8) -> Option<Self> {
        let mut parser = Text(bytes);
        let name = parser.name()?;
        let standard = Kind {
            name,
            offset: -parser.clock(true, 24)?,
            daylight: false,
        };
        if parser.0.is_empty() {
            return Some(Self {
                standard,
                daylight: None,
            });
        }
        let name = parser.name()?;
        let offset = if parser.0.first() == Some(&b',') {
            standard.offset.checked_add(3600)?
        } else {
            -parser.clock(true, 24)?
        };
        if !(-89_999..=93_599).contains(&offset) {
            return None;
        }
        let daylight = Kind {
            name,
            offset,
            daylight: true,
        };
        parser.byte(b',')?;
        let start = parser.rule(version)?;
        parser.byte(b',')?;
        let end = parser.rule(version)?;
        if !parser.0.is_empty() {
            return None;
        }
        Some(Self {
            standard,
            daylight: Some((daylight, start, end)),
        })
    }

    fn kind_at(&self, at: i64) -> Option<&Kind> {
        let Some((daylight, start, end)) = &self.daylight else {
            return Some(&self.standard);
        };
        let year = civil_from_days(at.div_euclid(DAY)).0;
        let mut latest: Option<(i128, bool)> = None;
        // Both late rules can spill into next January. Before they happen,
        // the latest event belongs to the nominal year two years earlier.
        for year in year - 2..=year + 1 {
            for (rule, before, is_daylight) in [
                (end, daylight.offset, false),
                (start, self.standard.offset, true),
            ] {
                let instant = rule.instant(year, before)?;
                if instant <= i128::from(at)
                    && latest.is_none_or(|previous| (instant, is_daylight) > previous)
                {
                    // Coincident end/start transitions leave DST in effect,
                    // including RFC 9636's all-year negative-DST example.
                    latest = Some((instant, is_daylight));
                }
            }
        }
        Some(if latest?.1 { daylight } else { &self.standard })
    }
}

struct Text<'a>(&'a [u8]);

impl Text<'_> {
    fn byte(&mut self, expected: u8) -> Option<()> {
        self.0 = self.0.strip_prefix(&[expected])?;
        Some(())
    }

    fn name(&mut self) -> Option<Vec<u8>> {
        let quoted = self.0.first() == Some(&b'<');
        if quoted {
            self.byte(b'<')?;
        }
        let count = self
            .0
            .iter()
            .take_while(|byte| {
                byte.is_ascii_alphabetic()
                    || (quoted && (byte.is_ascii_digit() || matches!(byte, b'+' | b'-')))
            })
            .count();
        if !(3..=MAX_DESIGNATION).contains(&count) {
            return None;
        }
        let (name, tail) = self.0.split_at_checked(count)?;
        self.0 = tail;
        if quoted {
            self.byte(b'>')?;
        }
        Some(name.to_vec())
    }

    fn number(&mut self, maximum: u16) -> Option<u16> {
        let mut result = 0u16;
        let mut count = 0;
        while let Some(byte) = self.0.first().copied().filter(u8::is_ascii_digit) {
            result = result
                .checked_mul(10)?
                .checked_add(u16::from(byte - b'0'))?;
            count += 1;
            if count > 3 || result > maximum {
                return None;
            }
            self.0 = self.0.get(1..)?;
        }
        (count > 0).then_some(result)
    }

    fn clock(&mut self, signed: bool, max_hour: u16) -> Option<i32> {
        let sign = match self.0.first() {
            Some(b'-') if signed => {
                self.byte(b'-')?;
                -1
            }
            Some(b'+') if signed => {
                self.byte(b'+')?;
                1
            }
            _ => 1,
        };
        let hour = i32::from(self.number(max_hour)?);
        let minute = if self.0.first() == Some(&b':') {
            self.byte(b':')?;
            i32::from(self.number(59)?)
        } else {
            0
        };
        let second = if self.0.first() == Some(&b':') {
            self.byte(b':')?;
            i32::from(self.number(59)?)
        } else {
            0
        };
        Some(sign * (hour * 3600 + minute * 60 + second))
    }

    fn rule(&mut self, version: u8) -> Option<Rule> {
        let date = match self.0.first() {
            Some(b'M') => {
                self.byte(b'M')?;
                let month = u8::try_from(self.number(12)?).ok()?;
                self.byte(b'.')?;
                let week = u8::try_from(self.number(5)?).ok()?;
                self.byte(b'.')?;
                let weekday = u8::try_from(self.number(6)?).ok()?;
                if month == 0 || week == 0 {
                    return None;
                }
                Date::Month {
                    month,
                    week,
                    weekday,
                }
            }
            Some(b'J') => {
                self.byte(b'J')?;
                let day = self.number(365)?;
                if day == 0 {
                    return None;
                }
                Date::Julian(day)
            }
            _ => Date::Ordinal(self.number(365)?),
        };
        let seconds = if self.0.first() == Some(&b'/') {
            self.byte(b'/')?;
            self.clock(version == b'3', if version == b'3' { 167 } else { 24 })?
        } else {
            7200
        };
        Some(Rule { date, seconds })
    }
}

impl Rule {
    fn instant(self, year: i64, before: i32) -> Option<i128> {
        let day = match self.date {
            Date::Julian(day) => {
                days_from_civil(year, 1, 1)
                    + i128::from(day - 1)
                    + i128::from(leap(year) && day >= 60)
            }
            Date::Ordinal(day) => days_from_civil(year, 1, 1) + i128::from(day),
            Date::Month {
                month,
                week,
                weekday,
            } => {
                let first = days_from_civil(year, month, 1);
                let mut day = (i128::from(weekday) - (first + 4).rem_euclid(7)).rem_euclid(7)
                    + i128::from(week - 1) * 7;
                let length = month_days(year, month)?;
                if day >= i128::from(length) {
                    day -= 7;
                }
                first + day
            }
        };
        Some(day * i128::from(DAY) + i128::from(self.seconds) - i128::from(before))
    }
}

fn leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn month_days(year: i64, month: u8) -> Option<u8> {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => Some(31),
        4 | 6 | 9 | 11 => Some(30),
        2 => Some(if leap(year) { 29 } else { 28 }),
        _ => None,
    }
}

fn days_from_civil(year: i64, month: u8, day: u8) -> i128 {
    let year = i128::from(year) - i128::from(month <= 2);
    let era = year.div_euclid(400);
    let y = year.rem_euclid(400);
    let m = i128::from(month) + if month > 2 { -3 } else { 9 };
    era * 146_097 + y * 365 + y / 4 - y / 100 + (153 * m + 2) / 5 + i128::from(day) - 1 - 719_468
}

/// Proleptic Gregorian civil date from signed Unix days.
pub fn civil_from_days(days: i64) -> (i64, u8, u8) {
    // Wide arithmetic also admits the complete signed-day input range.
    let shifted = i128::from(days) + 719_468;
    let era = shifted.div_euclid(146_097);
    let d = shifted.rem_euclid(146_097);
    let y = (d - d / 1460 + d / 36_524 - d / 146_096) / 365;
    let day_of_year = d - (365 * y + y / 4 - y / 100);
    let m = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * m + 2) / 5 + 1;
    let month = if m < 10 { m + 3 } else { m - 9 };
    (
        (y + era * 400 + i128::from(month <= 2)) as i64,
        month as u8,
        day as u8,
    )
}

#[cfg(test)]
pub(crate) fn fixture(times: &[(i64, u8)], kinds: &[(i32, bool, &str)], footer: &str) -> Vec<u8> {
    fn header(out: &mut Vec<u8>, times: usize, kinds: usize, names: usize) {
        out.extend_from_slice(b"TZif3");
        out.extend_from_slice(&[0; 15]);
        for count in [0, 0, 0, times, kinds, names] {
            out.extend_from_slice(&(count as u32).to_be_bytes());
        }
    }
    let mut out = Vec::new();
    header(&mut out, 0, 1, 1);
    out.extend_from_slice(&[0; 7]);
    let names: Vec<u8> = kinds
        .iter()
        .flat_map(|(_, _, name)| name.bytes().chain([0]))
        .collect();
    header(&mut out, times.len(), kinds.len(), names.len());
    for (time, _) in times {
        out.extend_from_slice(&time.to_be_bytes());
    }
    for (_, index) in times {
        out.push(*index);
    }
    let mut index = 0;
    for (offset, daylight, name) in kinds {
        out.extend_from_slice(&offset.to_be_bytes());
        out.push(u8::from(*daylight));
        out.push(index as u8);
        index += name.len() + 1;
    }
    out.extend_from_slice(&names);
    out.push(b'\n');
    out.extend_from_slice(footer.as_bytes());
    out.push(b'\n');
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn at(year: i64, month: u8, day: u8, seconds: i64) -> i64 {
        (days_from_civil(year, month, day) * i128::from(DAY)) as i64 + seconds
    }

    fn future(text: &str) -> Zone {
        Zone::parse(&fixture(&[], &[(0, false, "UTC")], text)).unwrap()
    }

    #[test]
    fn daylight_rules_cross_exact_boundaries_in_both_hemispheres() {
        let london = future("GMT0BST,M3.5.0/1,M10.5.0");
        let spring = at(2100, 3, 28, 3600);
        let autumn = at(2100, 10, 31, 3600);
        for (time, offset) in [
            (spring - 1, 0),
            (spring, 3600),
            (autumn - 1, 3600),
            (autumn, 0),
        ] {
            assert_eq!(london.offset_at(time), Some(offset), "{time}");
        }
        let auckland = future("NZST-12NZDT,M9.5.0,M4.1.0/3");
        assert_eq!(auckland.offset_at(at(2100, 1, 1, 0)), Some(46_800));
        assert_eq!(auckland.offset_at(at(2100, 7, 1, 0)), Some(43_200));
        let lord_howe = future("<+1030>-10:30<+11>-11,M10.1.0,M4.1.0");
        assert_eq!(lord_howe.offset_at(at(2100, 1, 1, 0)), Some(39_600));
        assert_eq!(lord_howe.offset_at(at(2100, 7, 1, 0)), Some(37_800));
    }

    #[test]
    fn negative_dst_and_signed_rules_include_all_year_and_new_year() {
        let dublin = future("IST-1GMT0,M10.5.0,M3.5.0/1");
        assert_eq!(dublin.offset_at(at(2100, 1, 1, 0)), Some(0));
        assert_eq!(dublin.offset_at(at(2100, 7, 1, 0)), Some(3600));
        let perpetual = future("XXX3EDT4,0/0,J365/23");
        for year in [1969, 1970, 2000, 2100] {
            for offset in -2 * DAY..=2 * DAY {
                assert_eq!(perpetual.offset_at(at(year, 1, 1, offset)), Some(-14_400));
            }
        }
        let late_pair = future("STD0DST,J365/167,J365/167");
        for (instant, expected) in [
            (at(2100, 1, 1, 0), 3600),
            (at(2100, 1, 6, 22 * 3600) - 1, 3600),
            (at(2100, 1, 6, 22 * 3600), 0),
            (at(2100, 1, 6, 23 * 3600) - 1, 0),
            (at(2100, 1, 6, 23 * 3600), 3600),
        ] {
            assert_eq!(late_pair.offset_at(instant), Some(expected));
        }
        assert_eq!(
            future("STD0DST,J364/167,J365/167").offset_at(at(2100, 1, 1, 0)),
            Some(0),
        );
        let mut v2 = fixture(&[], &[(0, false, "UTC")], "<-24>24<-23>23,J365/24,J365/24");
        v2[4] = b'2';
        v2[55] = b'2';
        assert_eq!(
            Zone::parse(&v2)
                .unwrap()
                .offset_at(at(2026, 1, 1, 12 * 3600)),
            Some(-82_800)
        );
        let early = future("STD0DST,J1/-2,J200");
        let transition = at(2099, 12, 31, 22 * 3600);
        assert_eq!(early.offset_at(transition - 1), Some(0));
        assert_eq!(early.offset_at(transition), Some(3600));
        let late = future("STD0DST,J100,J365/26");
        let transition = at(2101, 1, 1, 3600);
        assert_eq!(late.offset_at(transition - 1), Some(3600));
        assert_eq!(late.offset_at(transition), Some(0));
    }

    #[test]
    fn julian_rules_skip_leap_day_and_ordinal_rules_count_it() {
        for (rule, month, day) in [("STD0DST,J60,J300", 3, 1), ("STD0DST,59,300", 2, 29)] {
            let zone = future(rule);
            let transition = at(2000, month, day, 7200);
            assert_eq!(zone.offset_at(transition - 1), Some(0));
            assert_eq!(zone.offset_at(transition), Some(3600));
        }
    }

    #[test]
    fn transition_types_and_unknown_ranges_are_not_silent_utc() {
        let zone = Zone::parse(&fixture(
            &[(100, 1), (200, 0)],
            &[(7200, true, "DST"), (3600, false, "STD")],
            "",
        ))
        .unwrap();
        assert_eq!(zone.offset_at(-1), Some(7200));
        assert_eq!(zone.offset_at(99), Some(7200));
        assert_eq!(zone.offset_at(100), Some(3600));
        assert_eq!(zone.offset_at(199), Some(3600));
        assert_eq!(zone.offset_at(200), None);
        assert_eq!(future("<-00>0").offset_at(0), None);
        assert_eq!(
            Zone::parse(&fixture(&[], &[(0, false, "-00")], ""))
                .unwrap()
                .offset_at(0),
            None
        );
        assert_eq!(
            Zone::parse(&fixture(&[], &[(20_700, false, "+0545")], ""))
                .unwrap()
                .offset_at(0),
            Some(20_700)
        );
    }

    #[test]
    fn malformed_and_oversized_binary_data_refuses() {
        let valid = fixture(&[], &[(0, false, "UTC")], "UTC0");
        for end in 0..valid.len() {
            assert!(Zone::parse(&valid[..end]).is_none(), "prefix {end}");
        }
        assert!(Zone::parse(&vec![0; MAX_BYTES + 1]).is_none());
        for (position, byte) in [
            (0, b'X'),
            (4, b'4'),
            (55, b'2'),
            (95 + 4, 2),
            (95 + 5, 255),
            (95 + 9, b'X'),
        ] {
            let mut bad = valid.clone();
            bad[position] = byte;
            assert!(Zone::parse(&bad).is_none(), "mutation {position}");
        }
        for position in [51 + 20, 51 + 24, 51 + 28, 51 + 32, 51 + 36, 51 + 40] {
            let mut bad = valid.clone();
            bad[position..position + 4].copy_from_slice(&u32::MAX.to_be_bytes());
            assert!(Zone::parse(&bad).is_none(), "count {position}");
        }
        assert!(Zone::parse(&fixture(
            &[(100, 0), (100, 0)],
            &[(0, false, "UTC")],
            "UTC0"
        ))
        .is_none());
        assert!(Zone::parse(&fixture(
            &[(200, 0), (100, 0)],
            &[(0, false, "UTC")],
            "UTC0"
        ))
        .is_none());
        assert!(Zone::parse(&fixture(&[(100, 1)], &[(0, false, "UTC")], "UTC0")).is_none());
        assert!(Zone::parse(&fixture(&[(100, 0)], &[(3600, false, "UTC")], "UTC0")).is_none());
        assert!(Zone::parse(&fixture(&[(100, 0)], &[(0, false, "GMT")], "UTC0")).is_none());
        let mut leap = valid.clone();
        leap[51 + 28..51 + 32].copy_from_slice(&1u32.to_be_bytes());
        assert!(Zone::parse(&leap).is_none());
        let mut indicators = valid.clone();
        indicators[51 + 20..51 + 24].copy_from_slice(&1u32.to_be_bytes());
        indicators.insert(105, 1);
        assert!(
            Zone::parse(&indicators).is_none(),
            "UT requires a standard indicator"
        );
    }

    #[test]
    fn footer_grammar_is_complete_and_version_specific() {
        for text in [
            "UT0",
            "UTC",
            "UTC25",
            "UTC0junk",
            "UTC0DST",
            "UTC0DST,M0.1.0,M1.1.0",
            "UTC0DST,M1.6.0,M1.1.0",
            "UTC0DST,J0,J300",
            "UTC0DST,366,300",
            "UTC0DST,J1/168,J300",
            "UTC0DST,J1/2:60,J300",
            "UTC0\n",
            "UTC0\0",
            " UTC0",
            "UTC0 ",
            "<U/C>0",
        ] {
            assert!(
                Zone::parse(&fixture(&[], &[(0, false, "UTC")], text)).is_none(),
                "{text:?}"
            );
        }
        for text in ["STD0DST,J1/-1,J300", "STD0DST,J1/25,J300"] {
            let mut bytes = fixture(&[], &[(0, false, "UTC")], text);
            assert!(Zone::parse(&bytes).is_some());
            bytes[4] = b'2';
            bytes[55] = b'2';
            assert!(Zone::parse(&bytes).is_none());
        }
        for at in [i64::MIN, i64::MAX] {
            assert_eq!(future("UTC0").offset_at(at), Some(0));
            assert!(future("STD0DST,M3.2.0,M11.1.0").offset_at(at).is_some());
        }
    }

    #[test]
    fn signed_calendar_walks_leap_and_non_leap_centuries() {
        for (year, length) in [
            (1900, 365),
            (1972, 366),
            (2000, 366),
            (2024, 366),
            (2025, 365),
            (2100, 365),
        ] {
            let start = days_from_civil(year, 1, 1) as i64;
            let mut expected = (year, 1, 1);
            for offset in 0..length {
                assert_eq!(civil_from_days(start + offset), expected);
                let (year, month, day) = expected;
                // Independently advance explicit month lengths.
                let last = match month {
                    1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
                    4 | 6 | 9 | 11 => 30,
                    2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
                    _ => 28,
                };
                expected = if day < last {
                    (year, month, day + 1)
                } else if month < 12 {
                    (year, month + 1, 1)
                } else {
                    (year + 1, 1, 1)
                };
            }
            assert_eq!(expected, (year + 1, 1, 1));
        }
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }
}
