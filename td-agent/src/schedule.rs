//! When a schedule fires (DESIGN.md §3, Schedules): a five-field cron
//! expression or a single local time, and the instants either gives in a
//! zone, a local time that does not exist skipped and one that repeats
//! taken at its first instance.

use td_civil::{Civil, Zone};

/// How far ahead the next firing of a cron expression is looked for: the
/// Gregorian calendar's whole cycle, 400 years and a whole number of
/// weeks, so a date the expression allows is found if one ever comes and
/// none found means none ever will.
const HORIZON_DAYS: i64 = 146_097 + 1;

/// The last instant a firing is looked for after, the end of 9999 UTC:
/// past it td-civil's dates are clamped, and nothing would ever be later.
const LAST_AFTER: i64 = 253_402_300_799;

/// A five-field cron expression: minute, hour, day of month, month, day
/// of week, each the set of values it allows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    /// Whether each day field restricts: one that begins with `*` does
    /// not, as cron reads it.
    days_restricted: bool,
    weekdays_restricted: bool,
}

/// When a schedule fires.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum When {
    Cron(Cron),
    /// Once, at this local time.
    At(Civil),
}

/// One field's allowed values as a mask of bits `least..=most`, from
/// numbers, `*`, lists, ranges and steps, no names or macros.
fn field(text: &str, name: &str, least: u32, most: u32) -> Result<u64, String> {
    let wrong = |why: &str| format!("the {name} field {text:?} {why}");
    let number = |part: &str| -> Result<u32, String> {
        if part.is_empty() || part.len() > 2 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(wrong("has something that is not a number"));
        }
        let n: u32 = part
            .parse()
            .map_err(|_| wrong("has something that is not a number"))?;
        if !(least..=most).contains(&n) {
            return Err(wrong(&format!("has {n}, outside {least} to {most}")));
        }
        Ok(n)
    };
    if text.is_empty() {
        return Err(wrong("is empty"));
    }
    let mut mask = 0u64;
    for item in text.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => {
                // A step is an increment, not a value: `*/60` is minute 0.
                let step = Some(step)
                    .filter(|step| {
                        (1..=3).contains(&step.len()) && step.bytes().all(|b| b.is_ascii_digit())
                    })
                    .and_then(|step| step.parse::<u32>().ok())
                    .filter(|step| *step >= 1)
                    .ok_or_else(|| wrong("has a step that is not a number from 1"))?;
                (range, step)
            }
            None => (item, 1),
        };
        let (from, to) = if range == "*" {
            (least, most)
        } else if let Some((from, to)) = range.split_once('-') {
            let (from, to) = (number(from)?, number(to)?);
            if from > to {
                return Err(wrong("has a range that runs backwards"));
            }
            (from, to)
        } else {
            let from = number(range)?;
            // `N/S` steps from N to the field's end, as cron reads it.
            (from, if item.contains('/') { most } else { from })
        };
        let mut at = from;
        while at <= to {
            mask |= 1u64 << at;
            at = at.saturating_add(step);
        }
    }
    Ok(mask)
}

impl Cron {
    pub fn parse(text: &str) -> Result<Self, String> {
        let fields: Vec<&str> = text.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields.as_slice() else {
            return Err(format!(
                "a cron expression has five fields, minute hour day month weekday; {text:?} has {}",
                fields.len()
            ));
        };
        let weekdays = field(weekday, "day of week", 0, 7)?;
        // Both 0 and 7 are Sunday.
        let weekdays = (weekdays | (weekdays >> 7)) & 0x7f;
        Ok(Self {
            minutes: field(minute, "minute", 0, 59)?,
            hours: u32::try_from(field(hour, "hour", 0, 23)?).map_err(|e| e.to_string())?,
            days: u32::try_from(field(day, "day of month", 1, 31)?).map_err(|e| e.to_string())?,
            months: u16::try_from(field(month, "month", 1, 12)?).map_err(|e| e.to_string())?,
            weekdays: u8::try_from(weekdays).map_err(|e| e.to_string())?,
            days_restricted: !day.starts_with('*'),
            weekdays_restricted: !weekday.starts_with('*'),
        })
    }

    /// Whether the date is one this fires on: when both day fields
    /// restrict, either matching suffices, as in cron.
    fn on(&self, year: i64, month: u8, day: u8) -> bool {
        let days = td_civil::tzif::days_from_civil(year, month, day);
        // 1970-01-01 was a Thursday; Sunday is 0.
        let weekday = (days + 4).rem_euclid(7);
        let by_day = self.days & (1u32 << day) != 0;
        let by_weekday = self.weekdays & (1u8 << weekday) != 0;
        let by_month = self.months & (1u16 << month) != 0;
        by_month
            && if self.days_restricted && self.weekdays_restricted {
                by_day || by_weekday
            } else {
                by_day && by_weekday
            }
    }
}

impl When {
    /// A cron expression, or a single local time `YYYY-MM-DDTHH:MM`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.split_whitespace().count() == 5 {
            return Cron::parse(text).map(Self::Cron);
        }
        let civil = td_civil::parse_ymd_hm(text)
            .filter(|_| text.len() == 16 && text.as_bytes().get(10) == Some(&b'T'))
            .ok_or_else(|| {
                format!("{text:?} is neither a five-field cron expression nor a local time YYYY-MM-DDTHH:MM")
            })?;
        Ok(Self::At(civil))
    }

    /// The first instant after `after` this fires at in `zone`, if any:
    /// a local time in a gap is skipped, one that repeats fires at its
    /// first instance.
    pub fn next_after(&self, zone: &Zone, after: i64) -> Option<i64> {
        if after >= LAST_AFTER {
            return None;
        }
        match self {
            Self::At(civil) => zone.from_local_earliest(civil).filter(|at| *at > after),
            Self::Cron(cron) => {
                let (start, _) = zone.to_local(after);
                let first =
                    td_civil::tzif::days_from_civil(i64::from(start.year), start.month, start.day);
                let first = i64::try_from(first).ok()?;
                for day in first..first.saturating_add(HORIZON_DAYS) {
                    let (year, month, date) = td_civil::tzif::civil_from_days(day);
                    if !cron.on(year, month, date) {
                        continue;
                    }
                    let year = i32::try_from(year).ok()?;
                    for hour in (0..24u8).filter(|h| cron.hours & (1u32 << h) != 0) {
                        for minute in (0..60u8).filter(|m| cron.minutes & (1u64 << m) != 0) {
                            let local = Civil::new(year, month, date, hour, minute, 0);
                            match zone.from_local_earliest(&local) {
                                Some(at) if at > after => return Some(at),
                                _ => {}
                            }
                        }
                    }
                }
                None
            }
        }
    }

    /// The next `count` instants after `after`, fewer when it stops.
    pub fn upcoming(&self, zone: &Zone, after: i64, count: usize) -> Vec<i64> {
        let mut found = Vec::with_capacity(count);
        let mut from = after;
        while found.len() < count {
            let Some(at) = self.next_after(zone, from) else {
                break;
            };
            found.push(at);
            from = at;
        }
        found
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn utc(text: &str) -> i64 {
        td_civil::civil_to_unix_utc(&td_civil::parse_ymd_hm(text).unwrap()).unwrap()
    }

    fn shown(at: i64) -> String {
        td_civil::format_rfc3339_utc(at)
    }

    #[test]
    fn a_cron_expression_is_five_fields_of_numbers_lists_ranges_and_steps() {
        let cron = Cron::parse("*/15 9-17 * * 1-5").unwrap();
        assert_eq!(cron.minutes, 1 | 1 << 15 | 1 << 30 | 1 << 45);
        assert_eq!(cron.hours, (9..=17).fold(0, |m, h| m | 1 << h));
        assert_eq!(cron.weekdays, 0b0011_1110);
        assert!(!cron.days_restricted && cron.weekdays_restricted);
        // 7 is Sunday as 0 is; `N/S` steps to the field's end.
        assert_eq!(Cron::parse("0 0 * * 7").unwrap().weekdays, 1);
        // A step past the field's end leaves only its start.
        assert_eq!(Cron::parse("*/60 * * * *").unwrap().minutes, 1);
        assert_eq!(Cron::parse("0 */24 * * *").unwrap().hours, 1);
        assert_eq!(Cron::parse("0 0 * * 0,7").unwrap().weekdays, 1);
        assert_eq!(
            Cron::parse("50/5 0 * * *").unwrap().minutes,
            1 << 50 | 1 << 55
        );
        assert_eq!(
            Cron::parse("1,2,10-12/2 0 * * *").unwrap().minutes,
            1 << 1 | 1 << 2 | 1 << 10 | 1 << 12
        );
        for (text, why) in [
            ("* * * *", "five fields"),
            ("* * * * * *", "five fields"),
            ("60 * * * *", "outside 0 to 59"),
            ("* 24 * * *", "outside 0 to 23"),
            ("* * 0 * *", "outside 1 to 31"),
            ("* * * 13 *", "outside 1 to 12"),
            ("* * * * 8", "outside 0 to 7"),
            ("* * * jan *", "not a number"),
            ("@daily", "five fields"),
            ("5-1 * * * *", "backwards"),
            ("*/0 * * * *", "step"),
            ("*/+5 * * * *", "step"),
            ("*/1000 * * * *", "step"),
            ("1,,2 * * * *", "not a number"),
            ("-1 * * * *", "not a number"),
        ] {
            let err = Cron::parse(text).unwrap_err();
            assert!(err.contains(why), "{text}: {err}");
        }
    }

    #[test]
    fn a_when_is_a_cron_expression_or_one_local_time() {
        assert!(matches!(When::parse(" 0 9 * * 1 ").unwrap(), When::Cron(_)));
        assert_eq!(
            When::parse("2026-10-09T08:30").unwrap(),
            When::At(Civil::new(2026, 10, 9, 8, 30, 0))
        );
        for text in [
            "2026-10-09 08:30",
            "2026-10-09T8:30",
            "2026-02-30T08:30",
            "tomorrow",
        ] {
            assert!(When::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn the_next_firings_follow_the_expression_in_utc() {
        let zone = Zone::utc();
        let weekdays = When::parse("30 9 * * 1-5").unwrap();
        // 2026-10-09 is a Friday.
        let times: Vec<String> = weekdays
            .upcoming(&zone, utc("2026-10-09 10:00"), 3)
            .into_iter()
            .map(shown)
            .collect();
        assert_eq!(
            times,
            [
                "2026-10-12T09:30:00Z",
                "2026-10-13T09:30:00Z",
                "2026-10-14T09:30:00Z"
            ]
        );
        // Exactly at a firing, the next is the one after.
        assert_eq!(
            weekdays
                .next_after(&zone, utc("2026-10-12 09:30"))
                .map(shown),
            Some("2026-10-13T09:30:00Z".into())
        );
        // Both day fields restricted: either suffices, as in cron.
        let either = When::parse("0 0 13 * 5").unwrap();
        let times: Vec<String> = either
            .upcoming(&zone, utc("2026-11-01 00:00"), 3)
            .into_iter()
            .map(shown)
            .collect();
        assert_eq!(
            times,
            [
                "2026-11-06T00:00:00Z",
                "2026-11-13T00:00:00Z",
                "2026-11-20T00:00:00Z"
            ]
        );
        // A leap day is found across a century year that is not a leap
        // year, and one that must also be a Sunday decades on; a date
        // that never comes is never found, nor anything past 9999.
        let later = |text: &str, after: &str| {
            When::parse(text)
                .unwrap()
                .next_after(&zone, utc(after))
                .map(shown)
        };
        assert_eq!(
            later("0 0 29 2 */7", "2032-03-01 00:00"),
            Some("2060-02-29T00:00:00Z".into())
        );
        assert_eq!(
            later("0 0 */30 2 1", "2027-02-02 00:00"),
            Some("2038-02-01T00:00:00Z".into())
        );
        assert_eq!(
            When::parse("* * * * *")
                .unwrap()
                .next_after(&zone, i64::MAX),
            None
        );
        assert_eq!(
            When::parse("0 0 29 2 *")
                .unwrap()
                .next_after(&zone, utc("2097-03-01 00:00"))
                .map(shown),
            Some("2104-02-29T00:00:00Z".into())
        );
        assert_eq!(
            When::parse("0 0 31 2 *").unwrap().next_after(&zone, 0),
            None
        );
        // Once: before it, and never after.
        let once = When::parse("2026-10-09T08:30").unwrap();
        assert_eq!(
            once.upcoming(&zone, utc("2026-10-01 00:00"), 3)
                .into_iter()
                .map(shown)
                .collect::<Vec<_>>(),
            ["2026-10-09T08:30:00Z"]
        );
        assert_eq!(once.next_after(&zone, utc("2026-10-09 08:30")), None);
    }

    /// US Eastern for 2026 as a TZif v2 file (RFC 9636): clocks go forward
    /// an hour at 2026-03-08 02:00 local and back at 2026-11-01 02:00, the
    /// footer's rule after.
    fn new_york() -> Zone {
        fn header(out: &mut Vec<u8>, times: usize, kinds: usize, names: usize) {
            out.extend_from_slice(b"TZif2");
            out.extend_from_slice(&[0; 15]);
            for count in [0, 0, 0, times, kinds, names] {
                out.extend_from_slice(&(count as u32).to_be_bytes());
            }
        }
        let times = [(utc("2026-03-08 07:00"), 1u8), (utc("2026-11-01 06:00"), 0)];
        let kinds = [(-18_000i32, false, "EST"), (-14_400, true, "EDT")];
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
            out.push(index);
        }
        let mut index = 0;
        for (offset, daylight, name) in kinds {
            out.extend_from_slice(&offset.to_be_bytes());
            out.push(u8::from(daylight));
            out.push(index as u8);
            index += name.len() + 1;
        }
        out.extend_from_slice(&names);
        out.extend_from_slice(b"\nEST5EDT,M3.2.0,M11.1.0\n");
        Zone::from_tzif(&out).unwrap()
    }

    #[test]
    fn a_local_time_in_a_gap_is_skipped_and_one_that_repeats_fires_once() {
        let zone = new_york();
        // 02:30 does not exist on 2026-03-08: that day is skipped.
        let times: Vec<String> = When::parse("30 2 * * *")
            .unwrap()
            .upcoming(&zone, utc("2026-03-07 12:00"), 2)
            .into_iter()
            .map(shown)
            .collect();
        assert_eq!(times, ["2026-03-09T06:30:00Z", "2026-03-10T06:30:00Z"]);
        // 01:30 happens twice on 2026-11-01: it fires at the first.
        let times: Vec<String> = When::parse("30 1 * * *")
            .unwrap()
            .upcoming(&zone, utc("2026-10-31 12:00"), 2)
            .into_iter()
            .map(shown)
            .collect();
        assert_eq!(times, ["2026-11-01T05:30:00Z", "2026-11-02T06:30:00Z"]);
        // A frequent schedule is quiet through the repeated hour: after
        // 01:50 EDT it fires next at 02:00 EST, an hour and ten minutes on.
        assert_eq!(
            When::parse("*/15 * * * *")
                .unwrap()
                .next_after(&zone, utc("2026-11-01 05:50"))
                .map(shown),
            Some("2026-11-01T07:00:00Z".into())
        );
        // The walk starts from the local date: 22:30 EDT on 31 May is
        // 02:30 UTC on 1 June, and 23:00 that evening is still to come.
        assert_eq!(
            When::parse("0 23 * * *")
                .unwrap()
                .next_after(&zone, utc("2026-06-01 02:30"))
                .map(shown),
            Some("2026-06-01T03:00:00Z".into())
        );
        // A single time in the gap never fires.
        assert_eq!(
            When::parse("2026-03-08T02:30")
                .unwrap()
                .next_after(&zone, 0),
            None
        );
    }
}
