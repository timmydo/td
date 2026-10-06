//! Checked RFC 3339 projection of mail dates, without a time-zone DB.
use super::{month_days, Date, Offset};
use crate::{
    buffer::TextBuffer,
    decode_work::{Error as WorkError, Work},
    nfc::HeaderBudget,
    time::Tick,
    work::{Charge, Meter, Stop},
};

#[derive(Debug, Eq, PartialEq)]
pub enum Outcome<'a> {
    Date(&'a str),
    OutOfRange,
    /// The pinned positive-insertion table cannot qualify this :60 component.
    LeapSecondUnverified,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Capacity,
    Work(Stop),
    InterpretationLimit,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InterpretationLimit => f.write_str("date projection interpretation limit"),
            Self::Capacity => f.write_str("date output capacity"),
            Self::Work(error) => write!(f, "date projection work: {error}"),
            Self::InvalidState => f.write_str("invalid date projection state"),
        }
    }
}
impl std::error::Error for Error {}

impl From<WorkError> for Error {
    fn from(error: WorkError) -> Self {
        match error {
            WorkError::Work(stop) => Self::Work(stop),
            WorkError::InterpretationLimit => Self::InterpretationLimit,
            WorkError::InvalidState => Self::InvalidState,
        }
    }
}

/// Returns 20-byte UTC or 25-byte unknown-offset text in caller storage.
/// Revalidate public components; qualify a parsed leap second only through
/// the pinned insertion table after applying the offset convention.
pub fn render<'a>(
    date: Date,
    output: &'a mut [u8],
    now: Tick,
    work: &mut Meter,
) -> Result<Outcome<'a>, Error> {
    render_with_work(date, output, now, work)
}
/// Uses the original job and per-email budgets; output remains provisional.
/// This one-shot formatter retains no credit between separate projections.
pub fn render_with_budget<'a>(
    date: Date,
    output: &'a mut [u8],
    now: Tick,
    work: &mut Meter,
    budget: &mut HeaderBudget,
) -> Result<Outcome<'a>, Error> {
    render_with_work(
        date,
        output,
        now,
        &mut RenderWork {
            work,
            budget,
            credit: 0,
        },
    )
}
struct RenderWork<'w> {
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    credit: u8,
}
impl Work for RenderWork<'_> {
    fn charge(&mut self, now: Tick, charge: Charge) -> Result<(), WorkError> {
        if charge.io_bytes != 0
            || charge.unlinks != 0
            || (charge.records != 0 && charge.output_bytes != 0)
        {
            return Err(WorkError::InvalidState);
        }
        self.budget
            .charge_local(self.work, now, 0, charge.records, &mut self.credit)
            .map_err(WorkError::from)?;
        self.work
            .charge(
                now,
                Charge {
                    output_bytes: charge.output_bytes,
                    ..Charge::default()
                },
            )
            .map_err(WorkError::Work)
    }
}
fn render_with_work<'a>(
    date: Date,
    output: &'a mut [u8],
    now: Tick,
    work: &mut impl Work,
) -> Result<Outcome<'a>, Error> {
    // Component checks, at most five day steps, and fixed-width formatting.
    work.charge(
        now,
        Charge {
            records: 8,
            ..Charge::default()
        },
    )
    .map_err(Error::from)?;
    if !valid_components(date) {
        return Ok(Outcome::OutOfRange);
    }
    let leap = date.second == 60;
    if leap {
        // Date-key construction and at most five table comparisons.
        work.charge(
            now,
            Charge {
                records: 6,
                ..Charge::default()
            },
        )
        .map_err(Error::from)?;
    }
    let Some(date) = normalize(date) else {
        return Ok(if leap {
            Outcome::LeapSecondUnverified
        } else {
            Outcome::OutOfRange
        });
    };
    if leap && !listed_leap(date)? {
        return Ok(Outcome::LeapSecondUnverified);
    }
    let (suffix, length) = match date.offset {
        Offset::Known(0) => ("Z", 20),
        Offset::Unknown => ("-00:00", 25),
        Offset::Known(_) => return Err(Error::InvalidState),
    };
    let output = output.get_mut(..length).ok_or(Error::Capacity)?;
    work.charge(
        now,
        Charge {
            output_bytes: length as u64,
            ..Charge::default()
        },
    )
    .map_err(Error::from)?;
    {
        let mut text = TextBuffer::new(output);
        text.format(format_args!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}",
            date.year, date.month, date.day, date.hour, date.minute, date.second, suffix
        ))
        .map_err(|_| Error::InvalidState)?;
        if text.len() != length {
            return Err(Error::InvalidState);
        }
    }
    let text = std::str::from_utf8(output).map_err(|_| Error::InvalidState)?;
    Ok(Outcome::Date(text))
}
fn listed_leap(date: Date) -> Result<bool, Error> {
    if date.hour != 23 || date.minute != 59 {
        return Ok(false);
    }
    let key = u32::from(date.year) * 10000 + u32::from(date.month) * 100 + u32::from(date.day);
    let dates = &super::leap_dates::POSITIVE_DATES;
    let mut low = 0;
    let mut high = dates.len();
    if high > 31 {
        return Err(Error::InvalidState);
    }
    for _ in 0..5 {
        if low == high {
            return Ok(false);
        }
        let middle = low + (high - low) / 2;
        match key.cmp(dates.get(middle).ok_or(Error::InvalidState)?) {
            std::cmp::Ordering::Less => high = middle,
            std::cmp::Ordering::Equal => return Ok(true),
            std::cmp::Ordering::Greater => low = middle + 1,
        }
    }
    if low != high {
        return Err(Error::InvalidState);
    }
    Ok(false)
}
fn valid_components(date: Date) -> bool {
    date.year <= 9999
        && (1..=12).contains(&date.month)
        && date.day > 0
        && date.day <= month_days(date.year, date.month)
        && date.hour <= 23
        && date.minute <= 59
        && date.second <= 60
        && match date.offset {
            Offset::Unknown => true,
            Offset::Known(minutes) => (-5999..=5999).contains(&minutes),
        }
}
// Only render calls this, after validating raw components.
fn normalize(mut date: Date) -> Option<Date> {
    let offset = match date.offset {
        Offset::Unknown => return Some(date),
        Offset::Known(minutes) => i32::from(minutes),
    };
    let minutes = i32::from(date.hour) * 60 + i32::from(date.minute) - offset;
    let shift = minutes.div_euclid(1440);
    let minute = minutes.rem_euclid(1440);
    date.hour = u8::try_from(minute / 60).ok()?;
    date.minute = u8::try_from(minute % 60).ok()?;
    // The admitted offset bounds the day shift to -5..=5.
    for _ in 0..shift.unsigned_abs() {
        if shift < 0 {
            if date.day > 1 {
                date.day = date.day.checked_sub(1)?;
            } else {
                if date.month > 1 {
                    date.month = date.month.checked_sub(1)?;
                } else {
                    date.year = date.year.checked_sub(1)?;
                    date.month = 12;
                }
                date.day = month_days(date.year, date.month);
            }
        } else if date.day < month_days(date.year, date.month) {
            date.day = date.day.checked_add(1)?;
        } else {
            date.day = 1;
            if date.month < 12 {
                date.month = date.month.checked_add(1)?;
            } else {
                date.year = date.year.checked_add(1)?;
                date.month = 1;
            }
        }
    }
    if date.year > 9999 {
        return None;
    }
    date.offset = Offset::Known(0);
    Some(date)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{
        header_date::{Cursor, Status},
        time::Deadline,
    };
    fn work() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000,
                records: 100_000,
                output_bytes: 100_000,
                ..Charge::default()
            },
        )
    }
    fn parse(source: &[u8]) -> Date {
        let mut cursor = Cursor::new(source);
        for _ in 0..1000 {
            if let Status::Complete(date) = cursor.poll(Tick(1), &mut work()).unwrap() {
                return date.unwrap();
            }
        }
        panic!("date did not finish");
    }
    #[test]
    fn real_mail_dates_render_known_and_unknown_offsets_exactly() {
        for (source, expected) in [
            ("Fri, 21 Nov 1997 09:55:06 -0600", "1997-11-21T15:55:06Z"),
            ("1 Jan 2000 00:00 +0530", "1999-12-31T18:30:00Z"),
            ("31 Dec 1999 23:59:59 -0330", "2000-01-01T03:29:59Z"),
            ("1 Mar 2000 00:00 +0001", "2000-02-29T23:59:00Z"),
            ("1 Mar 1900 00:00 +0001", "1900-02-28T23:59:00Z"),
            ("1 Mar 2100 00:00 +0001", "2100-02-28T23:59:00Z"),
            ("1 Jan 1900 00:00 +0001", "1899-12-31T23:59:00Z"),
            ("1 Jan 2000 00:00 +9959", "1999-12-27T20:01:00Z"),
            ("31 Dec 1999 23:59 -9959", "2000-01-05T03:58:00Z"),
            ("1 Jan 2000 00:00 UT", "2000-01-01T00:00:00Z"),
            ("1 Jan 2000 00:00 -0000", "2000-01-01T00:00:00-00:00"),
            ("1 Jan 2000 00:00 Z", "2000-01-01T00:00:00-00:00"),
            ("1 Jan 2000 00:00 JST", "2000-01-01T00:00:00-00:00"),
        ] {
            let mut output = [0x5a; 26];
            let date = parse(source.as_bytes());
            let mut work = work();
            let before = work.remaining();
            assert_eq!(
                render(date, &mut output, Tick(1), &mut work),
                Ok(Outcome::Date(expected)),
                "{source}"
            );
            assert!(output[expected.len()..].iter().all(|&byte| byte == 0x5a));
            let after = work.remaining();
            assert_eq!(before.records - after.records, 8);
            assert_eq!(
                before.output_bytes - after.output_bytes,
                expected.len() as u64
            );
            assert_eq!(before.io_bytes, after.io_bytes);
        }
    }
    #[test]
    fn year_boundaries_and_invalid_public_components_refuse_without_output() {
        let zero = Date {
            year: 0,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            offset: Offset::Known(0),
        };
        let mut output = [0; 25];
        assert_eq!(
            render(zero, &mut output, Tick(1), &mut work()),
            Ok(Outcome::Date("0000-01-01T00:00:00Z"))
        );
        let last = Date {
            year: 9999,
            month: 12,
            day: 31,
            hour: 23,
            minute: 59,
            second: 59,
            ..zero
        };
        assert_eq!(
            render(last, &mut output, Tick(1), &mut work()),
            Ok(Outcome::Date("9999-12-31T23:59:59Z"))
        );
        for date in [
            Date {
                offset: Offset::Known(1),
                ..zero
            },
            Date {
                offset: Offset::Known(-1),
                ..last
            },
            Date {
                year: u16::MAX,
                ..zero
            },
            Date { month: 0, ..zero },
            Date { month: 13, ..zero },
            Date { day: 0, ..zero },
            Date { day: 32, ..zero },
            Date {
                year: 1900,
                month: 2,
                day: 29,
                ..zero
            },
            Date { hour: 24, ..zero },
            Date { minute: 60, ..zero },
            Date { second: 61, ..zero },
            Date {
                offset: Offset::Known(6000),
                ..zero
            },
            Date {
                offset: Offset::Known(-6000),
                ..zero
            },
            Date {
                offset: Offset::Known(i16::MIN),
                ..zero
            },
            Date {
                offset: Offset::Known(i16::MAX),
                ..zero
            },
        ] {
            let mut output = [0x5a; 25];
            let mut work = work();
            let before = work.remaining();
            assert_eq!(
                render(date, &mut output, Tick(1), &mut work),
                Ok(Outcome::OutOfRange)
            );
            assert_eq!(output, [0x5a; 25]);
            assert_eq!(before.output_bytes, work.remaining().output_bytes);
        }
    }
    #[test]
    fn leap_seconds_are_explicitly_unverified_without_repair_or_null() {
        for source in [
            "1 Jan 2000 12:34:60 +0000",
            "31 Dec 2016 23:58:60 +0000",
            "31 Dec 2016 23:58:60 -0000",
            "1 Jan 2017 00:59:60 -0000",
            "31 Dec 2016 23:59:60 +0001",
            "30 Jun 1971 23:59:60 +0000",
            "31 Dec 2026 23:59:60 +0000",
            "31 Dec 9999 23:59:60 -9959",
        ] {
            let date = parse(source.as_bytes());
            let mut output = [0x5a; 25];
            assert_eq!(
                render(date, &mut output, Tick(1), &mut work()),
                Ok(Outcome::LeapSecondUnverified)
            );
            assert_eq!(output, [0x5a; 25]);
            assert_eq!(date.second, 60);
            for invalid in [
                Date { month: 0, ..date },
                Date {
                    offset: Offset::Known(6000),
                    ..date
                },
            ] {
                assert_eq!(
                    render(invalid, &mut output, Tick(1), &mut work()),
                    Ok(Outcome::OutOfRange)
                );
                assert_eq!(output, [0x5a; 25]);
            }
        }
        let early = Date {
            year: 0,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 60,
            offset: Offset::Known(5999),
        };
        let mut output = [0x5a; 25];
        assert_eq!(
            render(early, &mut output, Tick(1), &mut work()),
            Ok(Outcome::LeapSecondUnverified)
        );
        assert_eq!(output, [0x5a; 25]);
    }
    #[test]
    fn pinned_insertions_render_without_clamping_and_charge_fixed_lookup_work() {
        for (source, expected) in [
            ("30 Jun 1972 23:59:60 +0000", "1972-06-30T23:59:60Z"),
            ("31 Dec 2016 23:59:60 +0000", "2016-12-31T23:59:60Z"),
            ("1 Jan 2017 00:59:60 +0100", "2016-12-31T23:59:60Z"),
            ("31 Dec 2016 18:29:60 -0530", "2016-12-31T23:59:60Z"),
            ("5 Jan 2017 03:58:60 +9959", "2016-12-31T23:59:60Z"),
            ("27 Dec 2016 20:00:60 -9959", "2016-12-31T23:59:60Z"),
            ("31 Dec 2016 23:59:60 -0000", "2016-12-31T23:59:60-00:00"),
            ("31 Dec 2016 23:59:60 Z", "2016-12-31T23:59:60-00:00"),
        ] {
            let date = parse(source.as_bytes());
            let mut output = [0x5a; 26];
            let mut work = work();
            let before = work.remaining();
            assert_eq!(
                render(date, &mut output, Tick(1), &mut work),
                Ok(Outcome::Date(expected)),
                "{source}"
            );
            assert!(output[expected.len()..].iter().all(|&b| b == 0x5a));
            assert_eq!(before.records - work.remaining().records, 14);
            assert_eq!(before.io_bytes, work.remaining().io_bytes);
            assert_eq!(
                before.output_bytes - work.remaining().output_bytes,
                expected.len() as u64
            );
        }
        for &key in &super::super::leap_dates::POSITIVE_DATES {
            let date = Date {
                year: (key / 10000) as u16,
                month: ((key / 100) % 100) as u8,
                day: (key % 100) as u8,
                hour: 23,
                minute: 59,
                second: 60,
                offset: Offset::Known(0),
            };
            let mut output = [0; 25];
            assert!(matches!(
                render(date, &mut output, Tick(1), &mut work()),
                Ok(Outcome::Date(_))
            ));
            assert_eq!(
                render(
                    Date {
                        day: date.day - 1,
                        ..date
                    },
                    &mut output,
                    Tick(1),
                    &mut work()
                ),
                Ok(Outcome::LeapSecondUnverified)
            );
        }
        // Every civil day around the table exercises absent-key binary-search paths.
        for year in 1971..=2018 {
            for month in 1..=12 {
                for day in 1..=month_days(year, month) {
                    let date = Date {
                        year,
                        month,
                        day,
                        hour: 23,
                        minute: 59,
                        second: 60,
                        offset: Offset::Known(0),
                    };
                    let key = u32::from(year) * 10000 + u32::from(month) * 100 + u32::from(day);
                    assert_eq!(
                        listed_leap(date).unwrap(),
                        super::super::leap_dates::POSITIVE_DATES.contains(&key)
                    );
                }
            }
        }
    }
    #[test]
    fn output_capacity_and_work_refusal_are_atomic() {
        for source in [
            b"1 Jan 2000 00:00 +0000".as_slice(),
            b"1 Jan 2000 00:00 -0000",
            b"31 Dec 2016 23:59:60 +0000",
            b"31 Dec 2016 23:59:60 -0000",
        ] {
            let date = parse(source);
            let size = if date.offset == Offset::Unknown {
                25
            } else {
                20
            };
            for capacity in 0..size {
                let mut output = [0x5a; 25];
                let mut work = work();
                let before = work.remaining();
                assert_eq!(
                    render(date, &mut output[..capacity], Tick(1), &mut work),
                    Err(Error::Capacity)
                );
                assert_eq!(output, [0x5a; 25]);
                assert_eq!(before.output_bytes, work.remaining().output_bytes);
            }
            let required = if date.second == 60 { 14 } else { 8 };
            for (records, output_bytes, now, expected) in [
                (required - 1, 100, 1, Stop::Records),
                (required, size as u64 - 1, 1, Stop::OutputBytes),
                (required, 100, 100, Stop::Deadline),
            ] {
                let mut work = Meter::new(
                    Deadline::after(Tick(0), 100).unwrap(),
                    Charge {
                        records,
                        output_bytes,
                        ..Charge::default()
                    },
                );
                let mut output = [0x5a; 25];
                assert_eq!(
                    render(date, &mut output, Tick(now), &mut work),
                    Err(Error::Work(expected))
                );
                assert_eq!(output, [0x5a; 25]);
                assert_eq!(
                    render(date, &mut output, Tick(1), &mut work),
                    Err(Error::Work(expected))
                );
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod budgeted_tests {
    use super::*;
    use crate::time::Deadline;
    fn work(output_bytes: u64) -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 2_000_000,
                output_bytes,
                ..Charge::default()
            },
        )
    }
    fn date() -> Date {
        Date {
            year: 2000,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            offset: Offset::Known(5999),
        }
    }
    fn leap() -> Date {
        Date {
            year: 2016,
            month: 12,
            day: 31,
            hour: 23,
            minute: 59,
            second: 60,
            offset: Offset::Known(0),
        }
    }
    fn limited(steps: u64) -> HeaderBudget {
        let mut budget = HeaderBudget::new();
        budget
            .charge_local(
                &mut work(1000),
                Tick(1),
                0,
                budget.steps_remaining() - steps,
                &mut 0,
            )
            .unwrap();
        budget
    }
    #[test]
    fn budgeted_formatting_preserves_results_and_exact_output_charges() {
        assert!(std::mem::size_of::<RenderWork<'_>>() <= 24);
        for (date, steps, expected) in [
            (date(), 8, Some("1999-12-27T20:01:00Z")),
            (
                Date {
                    offset: Offset::Unknown,
                    ..date()
                },
                8,
                Some("2000-01-01T00:00:00-00:00"),
            ),
            (leap(), 14, Some("2016-12-31T23:59:60Z")),
            (
                Date {
                    offset: Offset::Unknown,
                    ..leap()
                },
                14,
                Some("2016-12-31T23:59:60-00:00"),
            ),
            (
                Date {
                    year: 2020,
                    ..leap()
                },
                14,
                None,
            ),
            (Date { hour: 22, ..leap() }, 14, None),
            (
                Date {
                    year: u16::MAX,
                    ..date()
                },
                8,
                None,
            ),
            (Date { year: 0, ..date() }, 8, None),
        ] {
            let mut plain_output = [0xa5; 25];
            let mut output = [0xa5; 25];
            let mut plain_work = work(1000);
            let plain = render(date, &mut plain_output, Tick(1), &mut plain_work).unwrap();
            let mut work = work(1000);
            let mut budget = HeaderBudget::new();
            let before_steps = budget.steps_remaining();
            let before_bytes = budget.source_bytes_remaining();
            let value =
                render_with_budget(date, &mut output, Tick(1), &mut work, &mut budget).unwrap();
            assert_eq!(value, plain);
            match value {
                Outcome::Date(text) => assert_eq!(Some(text), expected),
                Outcome::OutOfRange | Outcome::LeapSecondUnverified => assert_eq!(expected, None),
            }
            assert_eq!(output, plain_output);
            assert_eq!(before_steps - budget.steps_remaining(), steps);
            assert_eq!(budget.source_bytes_remaining(), before_bytes);
            assert_eq!(work.remaining().records, 1_999_999);
            assert_eq!(
                work.remaining().output_bytes,
                1000 - expected.map_or(0, str::len) as u64
            );
            assert_eq!(
                work.remaining().output_bytes,
                plain_work.remaining().output_bytes
            );
        }
    }
    #[test]
    fn limits_capacity_and_deadline_refuse_before_output_mutation() {
        for (date, steps) in [(date(), 7), (leap(), 13)] {
            let mut work = work(1000);
            let mut budget = limited(steps);
            let mut output = [0xa5; 25];
            assert_eq!(
                render_with_budget(date, &mut output, Tick(1), &mut work, &mut budget),
                Err(Error::InterpretationLimit)
            );
            assert_eq!(output, [0xa5; 25]);
            assert_eq!(work.stopped(), None);
            let spent = if date.second == 60 { 8 } else { 0 };
            assert_eq!(budget.steps_remaining(), steps - spent);
            assert_eq!(work.remaining().records, 2_000_000 - u64::from(spent != 0));
            assert_eq!(work.remaining().output_bytes, 1000);
            let before = work.remaining();
            assert_eq!(
                render_with_budget(date, &mut output, Tick(1), &mut work, &mut budget),
                Err(Error::InterpretationLimit)
            );
            assert_eq!(work.remaining(), before);
        }
        let mut budget = HeaderBudget::new();
        let before_steps = budget.steps_remaining();
        let mut empty_records = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                output_bytes: 1000,
                ..Charge::default()
            },
        );
        let mut untouched = [0xa5; 25];
        assert_eq!(
            render_with_budget(
                date(),
                &mut untouched,
                Tick(1),
                &mut empty_records,
                &mut budget
            ),
            Err(Error::Work(Stop::Records))
        );
        assert_eq!(untouched, [0xa5; 25]);
        assert_eq!(budget.steps_remaining(), before_steps);
        let mut work = work(19);
        let mut output = [0xa5; 25];
        assert_eq!(
            render_with_budget(date(), &mut output, Tick(1), &mut work, &mut budget),
            Err(Error::Work(Stop::OutputBytes))
        );
        assert_eq!(output, [0xa5; 25]);
        assert_eq!(budget.steps_remaining(), before_steps - 8);
        assert_eq!(work.remaining().records, 1_999_999);
        assert_eq!(work.remaining().output_bytes, 19);
        let mut work = self::work(1000);
        assert_eq!(
            render_with_budget(date(), &mut output[..19], Tick(1), &mut work, &mut budget),
            Err(Error::Capacity)
        );
        assert_eq!(output, [0xa5; 25]);
        assert_eq!(work.remaining().output_bytes, 1000);
        assert_eq!(work.remaining().records, 1_999_999);
        assert_eq!(budget.steps_remaining(), before_steps - 16);
        let before_deadline = work.remaining();
        assert_eq!(
            render_with_budget(date(), &mut output, Tick(100), &mut work, &mut budget),
            Err(Error::Work(Stop::Deadline))
        );
        assert_eq!(output, [0xa5; 25]);
        assert_eq!(work.remaining(), before_deadline);
        assert_eq!(budget.steps_remaining(), before_steps - 16);
        let mut work = self::work(1000);
        let mut budget = HeaderBudget::new();
        let before = work.remaining();
        let steps = budget.steps_remaining();
        let mut adapter = RenderWork {
            work: &mut work,
            budget: &mut budget,
            credit: 0,
        };
        assert_eq!(
            adapter.charge(
                Tick(1),
                Charge {
                    records: 8,
                    output_bytes: 20,
                    ..Charge::default()
                }
            ),
            Err(WorkError::InvalidState)
        );
        assert_eq!(adapter.credit, 0);
        assert_eq!(work.remaining(), before);
        assert_eq!(budget.steps_remaining(), steps);
    }
    #[test]
    fn repeated_projection_discards_credit_without_resetting_budgets() {
        let mut work = work(1000);
        let mut budget = limited(16);
        let mut output = [0; 25];
        for _ in 0..2 {
            assert_eq!(
                render_with_budget(date(), &mut output, Tick(1), &mut work, &mut budget),
                Ok(Outcome::Date("1999-12-27T20:01:00Z"))
            );
        }
        assert_eq!(budget.steps_remaining(), 0);
        assert_eq!(work.remaining().records, 1_999_998);
        assert_eq!(work.remaining().output_bytes, 960);
        output.fill(0xa5);
        assert_eq!(
            render_with_budget(date(), &mut output, Tick(1), &mut work, &mut budget),
            Err(Error::InterpretationLimit)
        );
        assert_eq!(output, [0xa5; 25]);
    }
}
