//! One Content-ID value using the existing identifier grammar and conversion.
#[path = "content_id/json.rs"]
pub mod json;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Malformed,
    NestingLimit,
    Work(crate::work::Stop),
    InterpretationLimit,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed Content-ID value"),
            Self::NestingLimit => f.write_str("Content-ID comment nesting limit"),
            Self::Work(error) => write!(f, "Content-ID work: {error}"),
            Self::InterpretationLimit => f.write_str("Content-ID interpretation limit"),
            Self::InvalidState => f.write_str("invalid Content-ID state"),
        }
    }
}
impl std::error::Error for Error {}
impl From<crate::header_message_ids::Error> for Error {
    fn from(error: crate::header_message_ids::Error) -> Self {
        use crate::header_message_ids::Error as Inner;
        match error {
            Inner::Malformed => Self::Malformed,
            Inner::NestingLimit => Self::NestingLimit,
            Inner::Work(stop) => Self::Work(stop),
            Inner::InterpretationLimit => Self::InterpretationLimit,
            Inner::InvalidState => Self::InvalidState,
        }
    }
}
use crate::{header_message_ids::project, nfc::HeaderBudget, time::Tick, work::Meter};
pub use project::Status;
/// Complete-value validation precedes Begin/Scalar/End projection events.
/// Events remain provisional until Complete and fresh original admission.
/// Caller owns authorized input, retention and publication admission.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mime::content_id::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mime::content_id::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    inner: project::Budgeted<'a, 'w>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(source: &'a [u8], work: &'w mut Meter, budget: &'w mut HeaderBudget) -> Self {
        Self {
            inner: project::Budgeted::content_id(source, work, budget),
        }
    }
    pub fn is_complete(&self) -> bool {
        self.inner.is_complete()
    }
    pub fn is_encoding_problem(&self) -> Option<bool> {
        if self.is_complete() {
            Some(self.inner.is_encoding_problem())
        } else {
            None
        }
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.inner.check_deadline(now).map_err(Error::from)
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        self.inner.poll(now).map_err(Error::from)
    }
    pub fn finish(mut self, now: Tick) -> Result<(&'w mut Meter, &'w mut HeaderBudget), Error> {
        self.check_deadline(now)?;
        self.inner.finish().map_err(Error::from)
    }
}
const _: () =
    assert!(std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 512);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{
        time::Deadline,
        work::{Charge, Stop},
    };
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
    fn drain(cursor: &mut Cursor<'_, '_>) -> (Vec<Status>, Result<usize, Error>) {
        let mut events = Vec::new();
        for turn in 1..1_000_000 {
            let (before, steps) = cursor.inner.remaining();
            let status = cursor.poll(Tick(1));
            let (after, remaining) = cursor.inner.remaining();
            assert!(before.io_bytes - after.io_bytes <= 160);
            assert!(before.records - after.records <= 16);
            assert!(steps - remaining <= 255);
            assert!(before.output_bytes - after.output_bytes <= 4);
            match status {
                Ok(Status::Yield) => assert!(!cursor.is_complete()),
                Ok(Status::Complete) => {
                    assert!(cursor.is_complete());
                    assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                    assert_eq!(cursor.inner.remaining(), (after, remaining));
                    return (events, Ok(turn));
                }
                Ok(event) => events.push(event),
                Err(error) => {
                    assert!(!cursor.is_complete());
                    assert_eq!(cursor.is_encoding_problem(), None);
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                    assert_eq!(cursor.inner.remaining(), (after, remaining));
                    return (events, Err(error));
                }
            }
        }
        panic!("Content-ID did not finish")
    }
    #[test]
    fn literal_single_values_and_original_handoff() {
        for (source, expected, problem) in [
            ("(outer)< a (x). b @ c .d > (tail)", "a.b@c.d", false),
            ("<\"a\r\n b\"@[c\n\td]>", "\"a b\"@[c\td]", false),
            ("<e\u{301}@EXAMPLE>", "e\u{301}@EXAMPLE", false),
            ("<=?utf-8?Q?name?=@b>", "=?utf-8?Q?name?=@b", false),
            ("<\u{fdd0}@b>", "�@b", true),
        ] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let work_ptr = &work as *const Meter;
            let budget_ptr = &budget as *const HeaderBudget;
            let mut cursor = Cursor::new(source.as_bytes(), &mut work, &mut budget);
            assert_eq!(cursor.is_encoding_problem(), None);
            let (events, result) = drain(&mut cursor);
            assert!(result.is_ok());
            assert_eq!(events.first(), Some(&Status::Begin));
            assert_eq!(events.last(), Some(&Status::End));
            let mut wanted = vec![Status::Begin];
            wanted.extend(expected.chars().map(Status::Scalar));
            wanted.push(Status::End);
            assert_eq!(events, wanted);
            assert_eq!(cursor.is_encoding_problem(), Some(problem));
            let before = cursor.inner.remaining();
            cursor.check_deadline(Tick(1)).unwrap();
            assert_eq!(cursor.inner.remaining(), before);
            let (work, budget) = cursor.finish(Tick(1)).unwrap();
            assert!(std::ptr::eq(work, work_ptr));
            assert!(std::ptr::eq(budget, budget_ptr));
            assert_eq!(
                100_000_000 - work.remaining().output_bytes,
                2 * expected.len() as u64
            );
            assert_eq!(
                100_000_000 - work.remaining().records,
                (16_000_000 - budget.steps_remaining()).div_ceil(16)
            );
            assert_eq!(
                100_000_000 - work.remaining().io_bytes,
                16 * 1024 * 1024 - budget.source_bytes_remaining()
            );
        }
    }
    #[test]
    fn invalid_single_values_emit_no_projection() {
        let nested = format!("<a@b>{}", "(".repeat(33));
        for (source, expected) in [
            (b"".as_slice(), Error::Malformed),
            (b"(x)", Error::Malformed),
            (b"<a@b><c@d>", Error::Malformed),
            (b"<a@b> (x) <c@d>", Error::Malformed),
            (b"<a@b><bad>", Error::Malformed),
            (b"<a@b> (bad", Error::Malformed),
            (b"a@b", Error::Malformed),
            (b"<logo>", Error::Malformed),
            (b"old <a@b>", Error::Malformed),
            (b"<a@b> tail", Error::Malformed),
            (b"<a@\xff>", Error::Malformed),
            (nested.as_bytes(), Error::NestingLimit),
        ] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(source, &mut work, &mut budget);
            assert_eq!(drain(&mut cursor), (Vec::new(), Err(expected)));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(expected));
            assert_eq!(work.remaining().output_bytes, 100_000_000);
        }
        // The ordinary list constructor still accepts both identifiers.
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut list = project::Budgeted::new(
            b"<a@b><c@d>",
            crate::header_message_ids::Mode::Strict,
            &mut work,
            &mut budget,
        );
        let mut ends = 0;
        loop {
            match list.poll(Tick(1)).unwrap() {
                Status::End => ends += 1,
                Status::Complete => break,
                _ => {}
            }
        }
        assert_eq!(ends, 2);
    }
    #[test]
    fn every_header_and_job_cut_retires_provisional_events() {
        let source = "(x)<é@b> (tail)".as_bytes();
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let (expected, result) = drain(&mut Cursor::new(source, &mut work, &mut budget));
        assert!(result.is_ok());
        let visits = 16 * 1024 * 1024 - budget.source_bytes_remaining();
        let steps = 16_000_000 - budget.steps_remaining();
        let records = 100_000_000 - work.remaining().records;
        let output = 100_000_000 - work.remaining().output_bytes;
        let mut late_header = false;
        for (bytes, steps) in (0..visits)
            .map(|cut| (cut, steps))
            .chain((0..steps).map(|cut| (visits, cut)))
        {
            let mut work = meter();
            let mut budget = limited(bytes, steps);
            let (events, result) = drain(&mut Cursor::new(source, &mut work, &mut budget));
            assert_eq!(result, Err(Error::InterpretationLimit));
            late_header |= events.contains(&Status::End);
            assert!(expected.starts_with(&events));
            assert_eq!(work.stopped(), None);
            assert_eq!(
                Cursor::new(b"<a@b>", &mut work, &mut budget).poll(Tick(1)),
                Err(Error::InterpretationLimit)
            );
        }
        for (bytes, records, output, stop) in (0..visits)
            .map(|cut| (cut, records, output, Stop::IoBytes))
            .chain((0..records).map(|cut| (visits, cut, output, Stop::Records)))
            .chain((0..output).map(|cut| (visits, records, cut, Stop::OutputBytes)))
        {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: bytes,
                    records,
                    output_bytes: output,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            let (events, result) = drain(&mut Cursor::new(source, &mut work, &mut budget));
            assert_eq!(result, Err(Error::Work(stop)));
            assert!(expected.starts_with(&events));
            assert_eq!(work.stopped(), Some(stop));
        }
        assert!(late_header, "a header cut must retire an emitted End");
        let mut exact = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: visits,
                records,
                output_bytes: output,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let (events, result) = drain(&mut Cursor::new(source, &mut exact, &mut budget));
        assert_eq!(events, expected);
        assert!(result.is_ok());
        assert_eq!(exact.remaining().io_bytes, 0);
        assert_eq!(exact.remaining().records, 0);
        assert_eq!(exact.remaining().output_bytes, 0);
        let mut work = meter();
        let mut budget = limited(visits, steps);
        let (events, result) = drain(&mut Cursor::new(source, &mut work, &mut budget));
        assert_eq!(events, expected);
        assert!(result.is_ok());
        assert_eq!(budget.source_bytes_remaining(), 0);
        assert_eq!(budget.steps_remaining(), 0);
    }
    #[test]
    fn every_deadline_turn_and_late_completion_check_retire() {
        let source = b"<a@b> (tail)";
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
            assert_eq!(cursor.is_encoding_problem(), None);
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
        assert_eq!(cursor.is_encoding_problem(), None);
        assert_eq!(
            cursor.finish(Tick(1)).err(),
            Some(Error::Work(Stop::Deadline))
        );
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        assert!(drain(&mut cursor).1.is_ok());
        assert_eq!(
            cursor.finish(Tick(100)).err(),
            Some(Error::Work(Stop::Deadline))
        );
        for (now, error) in [
            (Tick(1), Error::InvalidState),
            (Tick(100), Error::Work(Stop::Deadline)),
        ] {
            assert_eq!(
                Cursor::new(source, &mut meter(), &mut HeaderBudget::new())
                    .finish(now)
                    .err(),
                Some(error)
            );
        }
    }
}
