//! Literal selected URI-reference spelling; field CFWS and word policy are external.
use crate::{
    admission::work::{Charge, Meter, Stop},
    decode_work::{Conversion, Error as InnerError, Lexical, Parsing, Work},
    nfc::HeaderBudget,
    ports::Tick,
};
use td_header::uri::{self, unfold};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    MalformedUri,
    MalformedFold,
    Work(Stop),
    InterpretationLimit,
    InvalidState,
}
impl From<InnerError> for Error {
    fn from(error: InnerError) -> Self {
        match error {
            InnerError::Work(stop) => Self::Work(stop),
            InnerError::InterpretationLimit => Self::InterpretationLimit,
            InnerError::InvalidState => Self::InvalidState,
        }
    }
}
impl From<uri::Error<InnerError>> for Error {
    fn from(error: uri::Error<InnerError>) -> Self {
        match error {
            uri::Error::Malformed => Self::MalformedUri,
            uri::Error::Work(error) => Self::from(error),
            uri::Error::InvalidState => Self::InvalidState,
        }
    }
}
impl From<unfold::Error<InnerError>> for Error {
    fn from(error: unfold::Error<InnerError>) -> Self {
        match error {
            unfold::Error::Malformed => Self::MalformedFold,
            unfold::Error::Work(error) => Self::from(error),
            unfold::Error::InvalidState => Self::InvalidState,
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MalformedUri => f.write_str("malformed literal URI reference"),
            Self::MalformedFold => f.write_str("malformed literal URI fold"),
            Self::Work(stop) => write!(f, "literal URI work: {stop}"),
            Self::InterpretationLimit => f.write_str("literal URI interpretation limit"),
            Self::InvalidState => f.write_str("invalid literal URI state"),
        }
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    /// Literal ASCII octet and offset in the supplied spelling slice.
    /// Provisional through completion and fresh original admission.
    Octet {
        byte: u8,
        position: usize,
    },
    Complete,
}
#[derive(Clone, Copy)]
enum Phase {
    Validate,
    Project,
    Complete,
}
/// Validate the complete unfolded URI reference before emitting literal octets.
/// Caller selects spelling outside CFWS/final ending and owns word placement.
/// Empty references are valid here; whole-field requirements stay external.
/// No percent decoding, word decoding, resolution or normalization occurs.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_location_literal::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_location_literal::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    source: &'a [u8],
    wire: unfold::Cursor<'a, InnerError>,
    validator: uri::Validator<InnerError>,
    phase: Phase,
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    credit: u8,
    failure: Option<Error>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub const fn new(source: &'a [u8], work: &'w mut Meter, budget: &'w mut HeaderBudget) -> Self {
        Self {
            source,
            wire: unfold::Cursor::new(source),
            validator: uri::Validator::reference(),
            phase: Phase::Validate,
            work,
            budget,
            credit: 0,
            failure: None,
        }
    }
    pub fn is_complete(&self) -> bool {
        self.failure.is_none() && matches!(self.phase, Phase::Complete)
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self
            .budget
            .charge(self.work, now, 0, 0, &mut self.credit)
            .map_err(InnerError::from)
            .map_err(Error::from);
        self.outcome(result)
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
        let result = self
            .work
            .charge(
                now,
                Charge {
                    output_bytes: bytes,
                    ..Charge::default()
                },
            )
            .map_err(Error::Work);
        self.outcome(result)
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete);
        }
        self.check_deadline(now)?;
        let result = self.step(now);
        self.outcome(result)
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        let mut parsing = Parsing::new(self.work, self.budget, &mut self.credit);
        let mut lexical = Lexical::new(&mut parsing, now);
        match (self.phase, self.wire.poll(&mut lexical)?) {
            (Phase::Validate | Phase::Project, unfold::Status::Yield) => Ok(Status::Yield),
            (Phase::Validate, unfold::Status::Octet { byte, .. }) => {
                self.validator.push(byte, &mut lexical)?;
                Ok(Status::Yield)
            }
            (Phase::Validate, unfold::Status::Complete) => {
                self.validator.finish()?;
                self.wire = unfold::Cursor::new(self.source);
                self.phase = Phase::Project;
                Ok(Status::Yield)
            }
            (Phase::Project, unfold::Status::Octet { byte, position }) => {
                Conversion::new(self.work, self.budget, &mut self.credit).charge(
                    now,
                    Charge {
                        output_bytes: 1,
                        ..Charge::default()
                    },
                )?;
                Ok(Status::Octet { byte, position })
            }
            (Phase::Project, unfold::Status::Complete) => {
                self.phase = Phase::Complete;
                Ok(Status::Complete)
            }
            // poll handles cached completion; private misuse remains a typed error.
            (Phase::Complete, _) => Err(Error::InvalidState),
        }
    }
    pub fn finish(mut self, now: Tick) -> Result<(&'w mut Meter, &'w mut HeaderBudget), Error> {
        self.check_deadline(now)?;
        if !self.is_complete() {
            return Err(Error::InvalidState);
        }
        Ok((self.work, self.budget))
    }
}
const _: () =
    assert!(std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 320);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::ports::Deadline;
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
    fn drain(cursor: &mut Cursor<'_, '_>) -> (Vec<(u8, usize)>, Result<usize, Error>) {
        let mut output = Vec::new();
        for turn in 1..100_000 {
            let before = cursor.work.remaining();
            let steps = cursor.budget.steps_remaining();
            let status = cursor.poll(Tick(1));
            let after = cursor.work.remaining();
            assert!(before.io_bytes - after.io_bytes <= 1);
            assert!(before.records - after.records <= 5);
            assert!(steps - cursor.budget.steps_remaining() <= 66);
            assert!(before.output_bytes - after.output_bytes <= 1);
            match status {
                Ok(Status::Yield) => assert!(!cursor.is_complete()),
                Ok(Status::Octet { byte, position }) => {
                    assert_eq!(cursor.source.get(position), Some(&byte));
                    assert!(!cursor.is_complete());
                    output.push((byte, position));
                }
                Ok(Status::Complete) => {
                    assert!(cursor.is_complete());
                    let steps = cursor.budget.steps_remaining();
                    assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                    assert_eq!(cursor.work.remaining(), after);
                    assert_eq!(cursor.budget.steps_remaining(), steps);
                    return (output, Ok(turn));
                }
                Err(error) => {
                    assert!(!cursor.is_complete());
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                    assert_eq!(cursor.work.remaining(), after);
                    return (output, Err(error));
                }
            }
        }
        panic!("literal URI did not finish")
    }
    #[test]
    fn complete_spelling_preserves_literal_octets_and_original_offsets() {
        for (source, wanted) in [
            (b"".as_slice(), b"".as_slice()),
            (b"../a%\r\n 2Fb?x#Y", b"../a%2Fb?x#Y"),
            (b"http://[::1]/a(b)", b"http://[::1]/a(b)"),
            (b"//[vF.a:!]/x", b"//[vF.a:!]/x"),
            (b"=?ascii?Q?file_name?=", b"=?ascii?Q?file_name?="),
        ] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let work_ptr = &work as *const Meter;
            let budget_ptr = &budget as *const HeaderBudget;
            let mut cursor = Cursor::new(source, &mut work, &mut budget);
            let (events, result) = drain(&mut cursor);
            assert!(result.is_ok());
            assert_eq!(
                events.iter().map(|(byte, _)| *byte).collect::<Vec<_>>(),
                wanted
            );
            let expected = source
                .iter()
                .enumerate()
                .filter_map(|(position, &byte)| {
                    (!matches!(byte, b' ' | b'\t' | b'\r' | b'\n')).then_some((byte, position))
                })
                .collect::<Vec<_>>();
            assert_eq!(events, expected);
            let (work, budget) = cursor.finish(Tick(1)).unwrap();
            assert!(std::ptr::eq(work, work_ptr));
            assert!(std::ptr::eq(budget, budget_ptr));
            assert_eq!(
                100_000_000 - work.remaining().io_bytes,
                2 * source.len() as u64
            );
            assert_eq!(
                100_000_000 - work.remaining().output_bytes,
                wanted.len() as u64
            );
            assert_eq!(
                100_000_000 - work.remaining().records,
                (16_000_000 - budget.steps_remaining()).div_ceil(16)
            );
        }
    }
    #[test]
    fn malformed_tails_emit_no_octets() {
        for (source, error) in [
            (b"a%".as_slice(), Error::MalformedUri),
            (b"../a%2X", Error::MalformedUri),
            (b"1g:h", Error::MalformedUri),
            (b"http://[:::]/", Error::MalformedUri),
            (b"x\xff", Error::MalformedUri),
            (b"a\r\n", Error::MalformedFold),
            (b"a\nX", Error::MalformedFold),
        ] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(source, &mut work, &mut budget);
            assert_eq!(drain(&mut cursor), (Vec::new(), Err(error)));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            assert_eq!(work.remaining().output_bytes, 100_000_000);
        }
    }
    #[test]
    fn every_allowance_cut_retires_projection_and_exact_costs_succeed() {
        let source = b"http://[::1]/a%\r\n 2Fb";
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let expected = drain(&mut Cursor::new(source, &mut work, &mut budget));
        assert!(expected.1.is_ok());
        let visits = 100_000_000 - work.remaining().io_bytes;
        let records = 100_000_000 - work.remaining().records;
        let output = 100_000_000 - work.remaining().output_bytes;
        let steps = 16_000_000 - budget.steps_remaining();
        assert_eq!(visits, 42);
        assert_eq!(output, 18);
        assert_eq!(steps, 168);
        assert_eq!(records, 11);
        let mut late = false;
        for (bytes, remaining) in (0..visits)
            .map(|cut| (cut, steps))
            .chain((0..steps).map(|cut| (visits, cut)))
        {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            budget
                .charge(
                    &mut meter(),
                    Tick(1),
                    budget.source_bytes_remaining() - bytes,
                    budget.steps_remaining() - remaining,
                    &mut 0,
                )
                .unwrap();
            let actual = drain(&mut Cursor::new(source, &mut work, &mut budget));
            assert!(expected.0.starts_with(&actual.0));
            late |= !actual.0.is_empty();
            assert_eq!(actual.1, Err(Error::InterpretationLimit));
            assert_eq!(
                Cursor::new(source, &mut work, &mut budget).poll(Tick(1)),
                Err(Error::InterpretationLimit)
            );
        }
        assert!(late);
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
            let actual = drain(&mut Cursor::new(source, &mut work, &mut budget));
            assert!(expected.0.starts_with(&actual.0));
            assert_eq!(actual.1, Err(Error::Work(stop)));
        }
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: visits,
                records,
                output_bytes: output,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        assert_eq!(
            drain(&mut Cursor::new(source, &mut work, &mut budget)),
            expected
        );
        assert_eq!(work.remaining().io_bytes, 0);
        assert_eq!(work.remaining().records, 0);
        assert_eq!(work.remaining().output_bytes, 0);
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        budget
            .charge(
                &mut meter(),
                Tick(1),
                budget.source_bytes_remaining() - visits,
                budget.steps_remaining() - steps,
                &mut 0,
            )
            .unwrap();
        assert_eq!(
            drain(&mut Cursor::new(source, &mut work, &mut budget)),
            expected
        );
        assert_eq!(budget.source_bytes_remaining(), 0);
        assert_eq!(budget.steps_remaining(), 0);
    }
    #[test]
    fn every_deadline_turn_and_fresh_final_admission_retire() {
        let source = b"../a";
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
            assert!(!cursor.is_complete());
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
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
