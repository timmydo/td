//! Caller-authorized URI spelling selection under original email admission.
use crate::{
    admission::work::{Charge, Meter, Stop},
    decode_work::{self, Admission, Lexical, Parsing},
    nfc::HeaderBudget,
    ports::Tick,
};
/// Source-relative ranges remain provisional through fresh original admission.
pub use td_header::uri::spelling::{Spelling, Status};
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
            Self::Malformed => "malformed leading URI CFWS",
            Self::NestingLimit => "URI spelling comment nesting limit",
            Self::Work(stop) => return write!(f, "URI spelling work refusal: {stop}"),
            Self::InterpretationLimit => "URI spelling interpretation refusal",
            Self::InvalidState => "invalid URI spelling state",
        })
    }
}
impl std::error::Error for Error {}
impl From<td_header::uri::spelling::Error<decode_work::Error>> for Error {
    fn from(error: td_header::uri::spelling::Error<decode_work::Error>) -> Self {
        use td_header::uri::spelling::Error as Shared;
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
/// One complete field-value slice under explicit surrounding-CFWS permission.
/// Caller selects the field and owns retention/publication admission.
/// Source excludes the final header ending; this phase proves no URI/fold validity.
/// Word placement and field presence remain external. Poll ranges are provisional;
/// retain only the range returned by freshly admitted consuming finish.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_location_selection::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_location_selection::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    inner: td_header::uri::spelling::Cursor<'a, decode_work::Error>,
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    credit: u8,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub const fn new(source: &'a [u8], work: &'w mut Meter, budget: &'w mut HeaderBudget) -> Self {
        Self {
            inner: td_header::uri::spelling::Cursor::new(source),
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
    /// Consuming discard only; the enclosing owner proves a syntax refusal and
    /// freshly admits these original budgets before returning or reusing them.
    pub(crate) fn discard(self) -> (&'w mut Meter, &'w mut HeaderBudget) {
        (self.work, self.budget)
    }
    #[cfg(test)]
    pub(crate) fn remaining(&self) -> (Charge, u64, u64) {
        (
            self.work.remaining(),
            self.budget.source_bytes_remaining(),
            self.budget.steps_remaining(),
        )
    }
    /// Crate-internal framing charge; the enclosing caller retires on refusal.
    pub(crate) fn charge_output(&mut self, now: Tick, bytes: u64) -> Result<(), Error> {
        self.check_deadline(now)?;
        self.work
            .charge(
                now,
                Charge {
                    output_bytes: bytes,
                    ..Charge::default()
                },
            )
            .map_err(Error::Work)
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if !self.inner.is_complete() {
            self.check_deadline(now)?;
        }
        self.inner
            .poll(&mut Lexical::new(
                &mut Parsing::new(self.work, self.budget, &mut self.credit),
                now,
            ))
            .map_err(Error::from)
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<(&'w mut Meter, &'w mut HeaderBudget, Spelling), Error> {
        self.check_deadline(now)?;
        let spelling = self.inner.finish().map_err(Error::from)?;
        Ok((self.work, self.budget, spelling))
    }
}
const _: () =
    assert!(std::mem::size_of::<td_header::uri::spelling::Cursor<'_, decode_work::Error>>() <= 160);
const _: () =
    assert!(std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 256);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{admission::work::Charge, ports::Deadline};
    fn meter() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 100_000_000,
                ..Charge::default()
            },
        )
    }
    fn limited(bytes: u64, steps: u64) -> HeaderBudget {
        let mut budget = HeaderBudget::new();
        budget
            .charge(
                &mut meter(),
                Tick(1),
                budget.source_bytes_remaining() - bytes,
                budget.steps_remaining() - steps,
                &mut 0,
            )
            .unwrap();
        budget
    }
    fn drain(cursor: &mut Cursor<'_, '_>) -> Result<(Spelling, usize), Error> {
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
                Ok(Status::Complete(spelling)) => {
                    assert!(cursor.is_complete());
                    assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete(spelling)));
                    assert_eq!(cursor.work.remaining(), after);
                    return Ok((spelling, turn));
                }
                Err(error) => {
                    assert!(!cursor.is_complete());
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                    assert_eq!(cursor.work.remaining(), after);
                    return Err(error);
                }
            }
        }
        panic!("mail URI spelling selector did not finish")
    }
    #[test]
    fn spelling_offsets_and_original_owner_handoff() {
        let source = b"(lead) a (b) c (tail)";
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let work_ptr = &work as *const Meter;
        let budget_ptr = &budget as *const HeaderBudget;
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        let (spelling, _) = drain(&mut cursor).unwrap();
        assert_eq!(spelling, Spelling { start: 7, end: 14 });
        assert_eq!(
            source.get(spelling.start..spelling.end),
            Some(b"a (b) c".as_slice())
        );
        let before = cursor.work.remaining();
        let steps = cursor.budget.steps_remaining();
        cursor.check_deadline(Tick(1)).unwrap();
        assert_eq!(cursor.work.remaining(), before);
        assert_eq!(cursor.budget.steps_remaining(), steps);
        let (work, budget, returned) = cursor.finish(Tick(1)).unwrap();
        assert_eq!(returned, spelling);
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
    fn selected_literal_replay_keeps_the_returned_original_owners() {
        use crate::mime_location_literal::{Cursor as Literal, Status as LiteralStatus};
        let source = b"(lead) ../a%\r\n 2Fb (tail)";
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 100_000_000,
                output_bytes: 100_000_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let work_ptr = &work as *const Meter;
        let budget_ptr = &budget as *const HeaderBudget;
        let mut selection = Cursor::new(source, &mut work, &mut budget);
        let (range, _) = drain(&mut selection).unwrap();
        assert_eq!(range, Spelling { start: 7, end: 18 });
        let (work, budget, returned) = selection.finish(Tick(1)).unwrap();
        assert_eq!(returned, range);
        let selected = source.get(range.start..range.end).unwrap();
        let before = work.remaining();
        let mut cursor = Literal::new(selected, work, budget);
        let mut output = Vec::new();
        let mut complete = false;
        for _ in 0..1000 {
            match cursor.poll(Tick(1)).unwrap() {
                LiteralStatus::Yield => {}
                LiteralStatus::Octet { byte, position } => {
                    assert_eq!(source.get(range.start + position), Some(&byte));
                    output.push(byte);
                }
                LiteralStatus::Complete => {
                    complete = true;
                    break;
                }
            }
        }
        assert!(complete);
        assert_eq!(output, b"../a%2Fb");
        let (work, budget) = cursor.finish(Tick(1)).unwrap();
        assert!(std::ptr::eq(work, work_ptr));
        assert!(std::ptr::eq(budget, budget_ptr));
        assert_eq!(
            before.io_bytes - work.remaining().io_bytes,
            2 * selected.len() as u64
        );
        assert_eq!(before.output_bytes - work.remaining().output_bytes, 8);
    }
    #[test]
    fn every_header_and_job_cut_retires_without_suffix_fallback() {
        for source in [
            b"(lead) a (b) c (tail)".as_slice(),
            b"a (bad",
            b"a (bad (tail)",
            b"a\r\nX",
        ] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let expected = drain(&mut Cursor::new(source, &mut work, &mut budget)).unwrap();
            let visits = 100_000_000 - work.remaining().io_bytes;
            let records = 100_000_000 - work.remaining().records;
            let steps = 16_000_000 - budget.steps_remaining();
            for (bytes, steps) in (0..visits)
                .map(|cut| (cut, steps))
                .chain((0..steps).map(|cut| (visits, cut)))
            {
                let mut work = meter();
                let mut budget = limited(bytes, steps);
                assert_eq!(
                    drain(&mut Cursor::new(source, &mut work, &mut budget)),
                    Err(Error::InterpretationLimit)
                );
                assert_eq!(work.stopped(), None);
                assert_eq!(
                    Cursor::new(b"a", &mut work, &mut budget).poll(Tick(1)),
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
                    drain(&mut Cursor::new(source, &mut work, &mut budget)),
                    Err(Error::Work(stop))
                );
                assert_eq!(work.stopped(), Some(stop));
                budget.charge(&mut meter(), Tick(1), 0, 0, &mut 0).unwrap();
            }
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: visits,
                    records,
                    ..Charge::default()
                },
            );
            let mut budget = limited(visits, steps);
            assert_eq!(
                drain(&mut Cursor::new(source, &mut work, &mut budget)),
                Ok(expected)
            );
            assert_eq!(work.remaining().io_bytes, 0);
            assert_eq!(work.remaining().records, 0);
            assert_eq!(budget.source_bytes_remaining(), 0);
            assert_eq!(budget.steps_remaining(), 0);
        }
    }
    #[test]
    fn every_live_deadline_and_fresh_handoff_retires_offsets() {
        for source in [b"a (tail)".as_slice(), b"a (bad", b"a (bad (tail)"] {
            let turns = drain(&mut Cursor::new(
                source,
                &mut meter(),
                &mut HeaderBudget::new(),
            ))
            .unwrap()
            .1;
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
                assert_eq!(
                    cursor.finish(Tick(1)).err(),
                    Some(Error::Work(Stop::Deadline))
                );
            }
        }
        let source = b"a (tail)";
        for check in [false, true] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(source, &mut work, &mut budget);
            drain(&mut cursor).unwrap();
            if check {
                assert_eq!(
                    cursor.check_deadline(Tick(100)),
                    Err(Error::Work(Stop::Deadline))
                );
                assert!(!cursor.is_complete());
            }
            assert_eq!(
                cursor.finish(Tick(100)).err(),
                Some(Error::Work(Stop::Deadline))
            );
        }
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
        drain(&mut cursor).unwrap();
        let bytes = cursor.budget.source_bytes_remaining() + 1;
        assert!(cursor
            .budget
            .charge(cursor.work, Tick(1), bytes, 0, &mut cursor.credit)
            .is_err());
        assert_eq!(
            cursor.check_deadline(Tick(1)),
            Err(Error::InterpretationLimit)
        );
        assert!(!cursor.is_complete());
        assert_eq!(
            cursor.finish(Tick(1)).err(),
            Some(Error::InterpretationLimit)
        );
    }
    #[test]
    fn empty_literal_invalid_and_four_byte_comment_cases() {
        for (source, range) in [
            (b"".as_slice(), Spelling { start: 0, end: 0 }),
            (b"a%", Spelling { start: 0, end: 2 }),
            (b"a (bad (tail)", Spelling { start: 0, end: 13 }),
            (b"a (bad ", Spelling { start: 0, end: 7 }),
            (b"(only)", Spelling { start: 6, end: 6 }),
        ] {
            assert_eq!(
                drain(&mut Cursor::new(
                    source,
                    &mut meter(),
                    &mut HeaderBudget::new()
                ))
                .unwrap()
                .0,
                range
            );
        }
        let deep = format!("{}x{}a", "(".repeat(33), ")".repeat(33));
        for (source, error) in [
            (b"(bad".as_slice(), Error::Malformed),
            (deep.as_bytes(), Error::NestingLimit),
        ] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(source, &mut work, &mut budget);
            assert_eq!(drain(&mut cursor), Err(error));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        }
        let source = format!("({})a(b)", "🐈".repeat(1024));
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(source.as_bytes(), &mut work, &mut budget);
        let mut maximum = (0, 0, 0);
        let mut complete = false;
        for _ in 0..100_000 {
            let before = cursor.work.remaining();
            let steps = cursor.budget.steps_remaining();
            match cursor.poll(Tick(1)).unwrap() {
                Status::Yield => {}
                Status::Complete(range) => {
                    assert_eq!(source.get(range.start..range.end), Some("a(b)"));
                    complete = true;
                }
            }
            let after = cursor.work.remaining();
            maximum.0 = maximum.0.max(before.io_bytes - after.io_bytes);
            maximum.1 = maximum.1.max(steps - cursor.budget.steps_remaining());
            maximum.2 = maximum.2.max(before.records - after.records);
            if complete {
                break;
            }
        }
        assert!(complete);
        assert_eq!(maximum, (160, 192, 12));
    }
}
