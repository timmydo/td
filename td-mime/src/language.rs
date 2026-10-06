//! Provisional Content-Language tag extents under original email admission.
#[path = "language/json.rs"]
pub mod json;
use crate::{
    decode_work::{self, Admission, Lexical, Parsing},
    nfc::HeaderBudget,
    time::Tick,
    work::{Meter, Stop},
};
pub use td_header::language_list::{Extent, Status};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Malformed,
    NestingLimit,
    Work(Stop),
    InterpretationLimit,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Malformed => "malformed Content-Language value",
            Self::NestingLimit => "Content-Language comment nesting limit",
            Self::Work(_) => "Content-Language work refusal",
            Self::InterpretationLimit => "Content-Language interpretation refusal",
            Self::InvalidState => "invalid Content-Language state",
        })
    }
}
impl std::error::Error for Error {}
impl From<td_header::language_list::Error<decode_work::Error>> for Error {
    fn from(error: td_header::language_list::Error<decode_work::Error>) -> Self {
        use td_header::language_list::Error as Shared;
        match error {
            Shared::Malformed => Self::Malformed,
            Shared::NestingLimit => Self::NestingLimit,
            Shared::InvalidState => Self::InvalidState,
            Shared::Work(decode_work::Error::Work(stop)) => Self::Work(stop),
            Shared::Work(decode_work::Error::InterpretationLimit) => Self::InterpretationLimit,
            Shared::Work(decode_work::Error::InvalidState) => Self::InvalidState,
        }
    }
}
/// One complete selected value; every tag remains provisional until success.
/// Caller owns selection, retention admission and publication, never renewed here.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mime::language::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mime::language::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    inner: td_header::language_list::Cursor<'a, decode_work::Error>,
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    credit: u8,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub const fn new(source: &'a [u8], work: &'w mut Meter, budget: &'w mut HeaderBudget) -> Self {
        Self {
            inner: td_header::language_list::Cursor::new(source),
            work,
            budget,
            credit: 0,
        }
    }
    pub fn is_complete(&self) -> bool {
        self.inner.is_complete()
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.inner
            .check_work(&mut Admission::new(
                now,
                self.work,
                self.budget,
                &mut self.credit,
            ))
            .map_err(Error::from)
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if self.inner.is_complete() {
            return Ok(Status::Complete);
        }
        self.check_deadline(now)?;
        self.inner
            .poll(&mut Lexical::new(
                &mut Parsing::new(self.work, self.budget, &mut self.credit),
                now,
            ))
            .map_err(Error::from)
    }
    pub fn finish(mut self, now: Tick) -> Result<(&'w mut Meter, &'w mut HeaderBudget), Error> {
        self.check_deadline(now)?;
        if !self.inner.is_complete() {
            return Err(Error::InvalidState);
        }
        Ok((self.work, self.budget))
    }
}
const _: () =
    assert!(std::mem::size_of::<td_header::language_list::Cursor<'_, decode_work::Error>>() <= 192);
const _: () =
    assert!(std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 256);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{time::Deadline, work::Charge};
    fn meter() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 100_000_000,
                output_bytes: 100_000_000,
                ..Charge::default()
            },
        )
    }
    fn limited(bytes: u64, steps: u64) -> HeaderBudget {
        let mut budget = HeaderBudget::new();
        budget
            .charge_local(
                &mut meter(),
                Tick(1),
                budget.source_bytes_remaining() - bytes,
                budget.steps_remaining() - steps,
                &mut 0,
            )
            .unwrap();
        budget
    }
    fn drain(cursor: &mut Cursor<'_, '_>) -> (Vec<Extent>, Result<usize, Error>) {
        let mut tags = Vec::new();
        for turn in 1..100_000 {
            let before = cursor.work.remaining();
            let bytes = cursor.budget.source_bytes_remaining();
            let steps = cursor.budget.steps_remaining();
            let status = cursor.poll(Tick(1));
            let after = cursor.work.remaining();
            assert!(before.io_bytes - after.io_bytes <= 160);
            assert!(before.records - after.records <= 12);
            assert!(steps - cursor.budget.steps_remaining() <= 192);
            assert_eq!(
                bytes - cursor.budget.source_bytes_remaining(),
                before.io_bytes - after.io_bytes
            );
            assert_eq!(before.output_bytes, after.output_bytes);
            match status {
                Ok(Status::Yield) => assert!(!cursor.is_complete()),
                Ok(Status::Tag(extent)) => {
                    tags.push(extent);
                    assert!(!cursor.is_complete());
                }
                Ok(Status::Complete) => {
                    assert!(cursor.is_complete());
                    assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                    assert_eq!(cursor.work.remaining(), after);
                    return (tags, Ok(turn));
                }
                Err(error) => {
                    assert!(!cursor.is_complete());
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                    assert_eq!(cursor.work.remaining(), after);
                    return (tags, Err(error));
                }
            }
        }
        panic!("mail language cursor did not finish")
    }
    #[test]
    fn literal_tags_and_original_owner_handoff() {
        let source = b"(x) EN-us, fr (tail)";
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let work_ptr = &work as *const Meter;
        let budget_ptr = &budget as *const HeaderBudget;
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        let (tags, result) = drain(&mut cursor);
        assert!(result.is_ok());
        let values: Vec<&[u8]> = tags
            .iter()
            .map(|extent| source.get(extent.start..extent.end).unwrap())
            .collect();
        assert_eq!(values, vec![b"EN-us".as_slice(), b"fr"]);
        let before = cursor.work.remaining();
        cursor.check_deadline(Tick(1)).unwrap();
        assert_eq!(cursor.work.remaining(), before);
        let (work, budget) = cursor.finish(Tick(1)).unwrap();
        assert!(std::ptr::eq(work, work_ptr));
        assert!(std::ptr::eq(budget, budget_ptr));
        assert_eq!(
            100_000_000 - work.remaining().records,
            (16_000_000 - budget.steps_remaining()).div_ceil(16)
        );
        assert_eq!(
            16 * 1024 * 1024 - budget.source_bytes_remaining(),
            100_000_000 - work.remaining().io_bytes
        );
    }
    #[test]
    fn every_header_and_job_allowance_cut_stays_terminal() {
        let source = b"en-US, fr (tail)";
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let (expected, result) = drain(&mut Cursor::new(source, &mut work, &mut budget));
        assert!(result.is_ok());
        let visits = 16 * 1024 * 1024 - budget.source_bytes_remaining();
        let steps = 16_000_000 - budget.steps_remaining();
        let records = 100_000_000 - work.remaining().records;
        for (bytes, steps) in (0..visits)
            .map(|cut| (cut, steps))
            .chain((0..steps).map(|cut| (visits, cut)))
        {
            let mut work = meter();
            let mut budget = limited(bytes, steps);
            let (tags, result) = drain(&mut Cursor::new(source, &mut work, &mut budget));
            assert_eq!(result, Err(Error::InterpretationLimit));
            assert!(expected.starts_with(&tags));
            assert_eq!(work.stopped(), None);
            assert_eq!(
                Cursor::new(b"en", &mut work, &mut budget).poll(Tick(1)),
                Err(Error::InterpretationLimit)
            );
        }
        for (bytes, records, stop) in (0..visits)
            .map(|cut| (cut, records, Stop::IoBytes))
            .chain((0..records).map(|cut| (visits, cut, Stop::Records)))
        {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: bytes,
                    records,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            assert_eq!(
                drain(&mut Cursor::new(source, &mut work, &mut budget)).1,
                Err(Error::Work(stop))
            );
            assert_eq!(work.stopped(), Some(stop));
            budget
                .charge_local(&mut meter(), Tick(1), 0, 0, &mut 0)
                .unwrap();
        }
        let mut work = meter();
        let mut budget = limited(visits, steps);
        let (tags, result) = drain(&mut Cursor::new(source, &mut work, &mut budget));
        assert_eq!(tags, expected);
        assert!(result.is_ok());
        assert_eq!(budget.source_bytes_remaining(), 0);
        assert_eq!(budget.steps_remaining(), 0);
    }
    #[test]
    fn every_live_deadline_and_fresh_completion_retire_results() {
        let source = b"en, fr (tail)";
        let turns = drain(&mut Cursor::new(
            source,
            &mut meter(),
            &mut HeaderBudget::new(),
        ))
        .1
        .unwrap();
        for cut in 0..turns {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(source, &mut work, &mut budget);
            for _ in 0..cut {
                cursor.poll(Tick(1)).unwrap();
            }
            assert_eq!(cursor.poll(Tick(100)), Err(Error::Work(Stop::Deadline)));
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
            assert!(!cursor.is_complete());
        }
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        assert!(drain(&mut cursor).1.is_ok());
        assert_eq!(
            cursor.check_deadline(Tick(100)),
            Err(Error::Work(Stop::Deadline))
        );
        assert!(!cursor.is_complete());
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        assert!(drain(&mut cursor).1.is_ok());
        assert_eq!(
            cursor.finish(Tick(100)).err(),
            Some(Error::Work(Stop::Deadline))
        );
        assert_eq!(
            Cursor::new(source, &mut meter(), &mut HeaderBudget::new())
                .finish(Tick(1))
                .err(),
            Some(Error::InvalidState)
        );
        assert_eq!(
            Cursor::new(source, &mut meter(), &mut HeaderBudget::new())
                .finish(Tick(100))
                .err(),
            Some(Error::Work(Stop::Deadline))
        );
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        assert!(drain(&mut cursor).1.is_ok());
        let bytes = cursor.budget.source_bytes_remaining() + 1;
        assert!(cursor
            .budget
            .charge_local(cursor.work, Tick(1), bytes, 0, &mut cursor.credit)
            .is_err());
        assert_eq!(
            cursor.check_deadline(Tick(1)),
            Err(Error::InterpretationLimit)
        );
        assert!(!cursor.is_complete());
        assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
        assert_eq!(cursor.work.stopped(), None);
        assert_eq!(
            cursor.finish(Tick(1)).err(),
            Some(Error::InterpretationLimit)
        );
    }
    #[test]
    fn four_byte_comment_reaches_literal_turn_ceilings() {
        let source = format!("({})en{}", "🐈".repeat(1024), "-abcdefgh".repeat(1024));
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        assert!(
            drain(&mut Cursor::new(source.as_bytes(), &mut work, &mut budget))
                .1
                .is_ok()
        );
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(source.as_bytes(), &mut work, &mut budget);
        let mut maximum = (0, 0, 0);
        let mut complete = false;
        for _ in 0..100_000 {
            let before = cursor.work.remaining();
            let steps = cursor.budget.steps_remaining();
            let status = cursor.poll(Tick(1)).unwrap();
            let after = cursor.work.remaining();
            maximum.0 = maximum.0.max(before.io_bytes - after.io_bytes);
            maximum.1 = maximum.1.max(steps - cursor.budget.steps_remaining());
            maximum.2 = maximum.2.max(before.records - after.records);
            if status == Status::Complete {
                complete = true;
                break;
            }
        }
        assert!(complete);
        assert_eq!(maximum, (160, 192, 12));
    }
    #[test]
    fn malformed_tail_nesting_and_empty_values_are_not_success() {
        for (source, expected) in [
            (b"".as_slice(), Error::Malformed),
            (b"en,", Error::Malformed),
            (b"en (bad", Error::Malformed),
            (b"en;q=1", Error::Malformed),
        ] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(source, &mut work, &mut budget);
            assert_eq!(drain(&mut cursor).1, Err(expected));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(expected));
        }
        let nested = format!("en {}x{}", "(".repeat(33), ")".repeat(33));
        assert_eq!(
            drain(&mut Cursor::new(
                nested.as_bytes(),
                &mut meter(),
                &mut HeaderBudget::new()
            ))
            .1,
            Err(Error::NestingLimit)
        );
    }
}
