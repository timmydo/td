//! Selected boundary/charset ASCII metadata; source field authority is external.
use super::{Attribute, OctetStatus, Octets, Plan, Selection};
use crate::{
    admission::work::{Charge, Meter},
    decode_work,
    mime_charset::{Charset, Label},
    mime_fields::Kind,
    mime_value::Role,
    nfc::HeaderBudget,
    ports::Tick,
};
use td_header::mime_protocol::{Kind as Grammar, Validator};
// Parsing interprets logical comparison records as aggregate steps, not jobs.
const ALIAS_FEED_STEPS: u64 = Label::FEED_RECORDS;
// finish visits at most the same ten fixed alias entries as feed.
const ALIAS_FINISH_STEPS: u64 = ALIAS_FEED_STEPS;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Purpose {
    Boundary,
    Charset,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Value {
    Absent,
    Invalid,
    Present,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct End {
    pub value: Value,
    pub selection: Selection,
    pub bytes: usize,
    pub known_charset: Option<Charset>,
    pub unsupported_qualifier: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete(End),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Parameter(super::Error),
    OutputCapacity,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parameter(error) => write!(f, "MIME protocol parameter: {error}"),
            Self::OutputCapacity => f.write_str("MIME protocol parameter output capacity"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parameter(error) => Some(error),
            Self::OutputCapacity => None,
        }
    }
}
/// Passive bytes whose backing survives release of the original budgets.
pub struct Retained<'w> {
    pub end: End,
    value: Option<&'w [u8]>,
}
impl<'w> Retained<'w> {
    pub const fn value(&self) -> Option<&'w [u8]> {
        self.value
    }
}
/// Retains exact selected Data octets without charset conversion, word
/// recognition or NFC. Invalid selected values never reconsider siblings.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_parameter::protocol::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_parameter::protocol::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    source: Octets<'a>,
    validator: Validator,
    purpose: Purpose,
    qualifier: Label,
    label: Label,
    output: &'w mut [u8],
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    credit: u8,
    used: usize,
    overflow: bool,
    end: Option<End>,
    failure: Option<Error>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(
        source: &'a [u8],
        purpose: Purpose,
        output: &'w mut [u8],
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Self {
        let (attribute, grammar) = match purpose {
            Purpose::Boundary => (Attribute::Boundary, Grammar::Boundary),
            Purpose::Charset => (Attribute::Charset, Grammar::Token),
        };
        Self {
            source: Octets::new(source, Kind::ContentType, attribute),
            validator: Validator::new(grammar),
            purpose,
            qualifier: Label::new(),
            label: Label::new(),
            output,
            work,
            budget,
            credit: 0,
            used: 0,
            overflow: false,
            end: None,
            failure: None,
        }
    }
    /// Complete ASCII metadata only, with original spelling preserved.
    pub fn value(&self) -> Option<&[u8]> {
        let end = self.end?;
        if self.failure.is_some() || end.value != Value::Present {
            return None;
        }
        self.output.get(..end.bytes)
    }
    /// Healthy completion, fresh original admission and passive byte handoff.
    /// Enclosing metadata/job publication still requires its final admission.
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<(Retained<'w>, &'w mut Meter, &'w mut HeaderBudget), Error> {
        self.check_deadline(now)?;
        let end = self
            .end
            .ok_or(Error::Parameter(super::Error::InvalidState))?;
        let output: &'w [u8] = self.output;
        let value = if end.value == Value::Present {
            Some(
                output
                    .get(..end.bytes)
                    .ok_or(Error::Parameter(super::Error::InvalidState))?,
            )
        } else {
            None
        };
        Ok((Retained { end, value }, self.work, self.budget))
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
            self.end = None;
            self.used = 0;
        }
        result
    }
    fn admit(&mut self, now: Tick, steps: u64) -> Result<(), Error> {
        self.budget
            .charge(self.work, now, 0, steps, &mut self.credit)
            .map_err(decode_work::Error::from)
            .map_err(super::Error::from)
            .map_err(Error::Parameter)
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.admit(now, 0);
        self.outcome(result)
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if let Some(end) = self.end {
            return Ok(Status::Complete(end));
        }
        let result = self.step(now);
        self.outcome(result)
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        let status = self
            .source
            .poll_with_work(
                now,
                &mut decode_work::Parsing::new(self.work, self.budget, &mut self.credit),
            )
            .map_err(Error::Parameter)?;
        match status {
            OctetStatus::Yield => Ok(Status::Yield),
            OctetStatus::Octet { role, value } => {
                if role == Role::Charset {
                    self.admit(now, ALIAS_FEED_STEPS)?;
                    self.qualifier.feed(value);
                    return Ok(Status::Yield);
                }
                if role != Role::Data {
                    return Ok(Status::Yield);
                }
                self.admit(
                    now,
                    1 + if self.purpose == Purpose::Charset {
                        ALIAS_FEED_STEPS
                    } else {
                        0
                    },
                )?;
                if self.purpose == Purpose::Charset {
                    self.label.feed(value);
                }
                self.validator.feed(value);
                if self.validator.is_invalid() {
                    self.used = 0;
                    return Ok(Status::Yield);
                }
                if self.overflow {
                    return Ok(Status::Yield);
                }
                let next = self.used.checked_add(1).ok_or(Error::OutputCapacity)?;
                let Some(target) = self.output.get_mut(self.used..next) else {
                    self.overflow = true;
                    return Ok(Status::Yield);
                };
                self.work
                    .charge(
                        now,
                        Charge {
                            output_bytes: 1,
                            ..Charge::default()
                        },
                    )
                    .map_err(super::Error::Work)
                    .map_err(Error::Parameter)?;
                target.copy_from_slice(&[value]);
                self.used = next;
                Ok(Status::Yield)
            }
            OctetStatus::Complete(selection) => {
                self.admit(now, 2 * ALIAS_FINISH_STEPS)?;
                let qualified = matches!(
                    selection.plan,
                    Some(
                        Plan::Extended(_)
                            | Plan::Sections {
                                initial_encoded: true,
                                ..
                            }
                    )
                );
                let unsupported_qualifier = qualified && self.qualifier.finish().is_none();
                let value = if selection.plan.is_none() {
                    if self.used != 0 {
                        return Err(Error::Parameter(super::Error::InvalidState));
                    }
                    Value::Absent
                } else if self.validator.is_valid() {
                    if self.overflow {
                        return Err(Error::OutputCapacity);
                    }
                    Value::Present
                } else {
                    self.used = 0;
                    Value::Invalid
                };
                let end = End {
                    value,
                    selection,
                    bytes: self.used,
                    known_charset: if value == Value::Present && self.purpose == Purpose::Charset {
                        self.label.finish()
                    } else {
                        None
                    },
                    unsupported_qualifier,
                };
                self.end = Some(end);
                Ok(Status::Complete(end))
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{mime_parameter::Error as ParameterError, ports::Deadline};
    fn meter() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 1_000_000,
                records: 1_000_000,
                output_bytes: 1_000_000,
                ..Charge::default()
            },
        )
    }
    fn drain(cursor: &mut Cursor<'_, '_>) -> Result<End, Error> {
        for _ in 0..100_000 {
            match cursor.poll(Tick(1))? {
                Status::Yield => {}
                Status::Complete(end) => return Ok(end),
            }
        }
        panic!("protocol parameter did not finish")
    }
    #[test]
    fn exact_selection_ascii_grammar_and_passive_handoff() {
        assert!(
            std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 1280
        );
        for (source, purpose, expected, value, rejected, known, unsupported) in [
            (
                b"multipart/mixed;boundary=first;boundary=second".as_slice(),
                Purpose::Boundary,
                Some(b"first".as_slice()),
                Value::Present,
                false,
                None,
                false,
            ),
            (
                b"multipart/mixed;boundary=ordinary;boundary*=utf-8''a%20b",
                Purpose::Boundary,
                Some(b"a b"),
                Value::Present,
                false,
                None,
                false,
            ),
            (
                b"multipart/mixed;boundary=ordinary;boundary*=utf-8''a%20",
                Purpose::Boundary,
                None,
                Value::Invalid,
                false,
                None,
                false,
            ),
            (
                b"multipart/mixed;boundary=ordinary;boundary*=utf-8''%FF",
                Purpose::Boundary,
                None,
                Value::Invalid,
                false,
                None,
                false,
            ),
            (
                b"multipart/mixed;boundary=ordinary;boundary*=utf-8''%xx",
                Purpose::Boundary,
                Some(b"ordinary"),
                Value::Present,
                true,
                None,
                false,
            ),
            (
                b"multipart/mixed;boundary*1*=b;boundary*0*=utf-8'en'a",
                Purpose::Boundary,
                Some(b"ab"),
                Value::Present,
                false,
                None,
                false,
            ),
            (
                b"multipart/mixed;boundary*=unknown''ABC",
                Purpose::Boundary,
                Some(b"ABC"),
                Value::Present,
                false,
                None,
                true,
            ),
            (
                b"multipart/mixed;boundary*=''ABC",
                Purpose::Boundary,
                Some(b"ABC"),
                Value::Present,
                false,
                None,
                true,
            ),
            (
                b"multipart/mixed;boundary=\"=?utf-8?Q?x?=\"",
                Purpose::Boundary,
                Some(b"=?utf-8?Q?x?="),
                Value::Present,
                false,
                None,
                false,
            ),
            (
                b"multipart/mixed;boundary=\"a\\?b\"",
                Purpose::Boundary,
                Some(b"a?b"),
                Value::Present,
                false,
                None,
                false,
            ),
            (
                b"multipart/mixed;boundary=\"\"",
                Purpose::Boundary,
                None,
                Value::Invalid,
                false,
                None,
                false,
            ),
            (
                b"multipart/mixed;x=ignored",
                Purpose::Boundary,
                None,
                Value::Absent,
                false,
                None,
                false,
            ),
            (
                b"multipart/mixed;boundary=\"a\xc3\xa9\"",
                Purpose::Boundary,
                None,
                Value::Invalid,
                false,
                None,
                false,
            ),
            (
                b"text/plain;charset=\"utf\xc3\xa9\"",
                Purpose::Charset,
                None,
                Value::Invalid,
                false,
                None,
                false,
            ),
            (
                b"text/plain;charset*=x-foo''utf-8",
                Purpose::Charset,
                Some(b"utf-8"),
                Value::Present,
                false,
                Some(Charset::Utf8),
                true,
            ),
            (
                b"text/plain;charset*1*=8;charset*0*=x-foo'en'utf-",
                Purpose::Charset,
                Some(b"utf-8"),
                Value::Present,
                false,
                Some(Charset::Utf8),
                true,
            ),
            (
                b"multipart/mixed;boundary=(c)abc(d)",
                Purpose::Boundary,
                Some(b"abc"),
                Value::Present,
                false,
                None,
                false,
            ),
            (
                b"text/plain;charset=UtF-8",
                Purpose::Charset,
                Some(b"UtF-8"),
                Value::Present,
                false,
                Some(Charset::Utf8),
                false,
            ),
            (
                b"text/plain;charset=unknown",
                Purpose::Charset,
                Some(b"unknown"),
                Value::Present,
                false,
                None,
                false,
            ),
            (
                b"text/plain;charset=ascii;charset*=utf-8''%75tf%38",
                Purpose::Charset,
                Some(b"utf8"),
                Value::Present,
                false,
                Some(Charset::Utf8),
                false,
            ),
            (
                b"text/plain;charset=ascii;charset*=utf-8''utf%208",
                Purpose::Charset,
                None,
                Value::Invalid,
                false,
                None,
                false,
            ),
            (
                b"text/plain;charset=\"\"",
                Purpose::Charset,
                None,
                Value::Invalid,
                false,
                None,
                false,
            ),
            (
                b"text/plain",
                Purpose::Charset,
                None,
                Value::Absent,
                false,
                None,
                false,
            ),
        ] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let pointers = (&work as *const _, &budget as *const _);
            let mut output = [0; 70];
            let mut cursor = Cursor::new(source, purpose, &mut output, &mut work, &mut budget);
            assert!(cursor.value().is_none());
            let result = drain(&mut cursor);
            assert!(result.is_ok(), "{source:?}: {result:?}");
            let end = result.unwrap();
            assert_eq!(end.value, value, "{source:?}");
            assert_eq!(end.selection.invalid_extended, rejected);
            assert_eq!(end.known_charset, known);
            assert_eq!(end.unsupported_qualifier, unsupported);
            assert_eq!(cursor.value(), expected);
            assert_eq!(end.bytes, expected.map_or(0, <[u8]>::len));
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete(end)));
            let (retained, work, budget) = cursor.finish(Tick(1)).unwrap();
            let extracted = retained.value();
            let moved = retained;
            assert_eq!(moved.value(), extracted);
            assert_eq!(extracted, expected);
            assert_eq!(pointers, (work as *const _, budget as *const _));
        }
        for len in 0..=72 {
            let source = format!("multipart/mixed;boundary=\"{}\"", "x".repeat(len));
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut output = [0; 70];
            let mut cursor = Cursor::new(
                source.as_bytes(),
                Purpose::Boundary,
                &mut output,
                &mut work,
                &mut budget,
            );
            let end = drain(&mut cursor).unwrap();
            assert_eq!(
                end.value,
                if (1..=70).contains(&len) {
                    Value::Present
                } else {
                    Value::Invalid
                }
            );
        }
    }
    #[test]
    fn every_original_allowance_and_live_deadline_cut_is_sticky() {
        use crate::admission::work::Stop;
        let input = b"text/plain;charset=ascii;charset*=utf-8'en'%55tF-8";
        let mut work = meter();
        let original = work.remaining();
        let mut budget = HeaderBudget::new();
        let (source_initial, steps_initial) =
            (budget.source_bytes_remaining(), budget.steps_remaining());
        let mut output = [0; 70];
        let mut turns = 0;
        {
            let mut cursor =
                Cursor::new(input, Purpose::Charset, &mut output, &mut work, &mut budget);
            for _ in 0..100_000 {
                let before = (
                    cursor.budget.source_bytes_remaining(),
                    cursor.budget.steps_remaining(),
                    cursor.work.remaining(),
                );
                let status = cursor.poll(Tick(1)).unwrap();
                turns += 1;
                let after = (
                    cursor.budget.source_bytes_remaining(),
                    cursor.budget.steps_remaining(),
                    cursor.work.remaining(),
                );
                assert!(before.0 - after.0 <= 160);
                assert!(before.1 - after.1 <= 276);
                assert!(before.2.records - after.2.records <= 18);
                assert!(before.2.output_bytes - after.2.output_bytes <= 1);
                if matches!(status, Status::Complete(_)) {
                    break;
                }
            }
            assert!(cursor.end.is_some());
        }
        let final_work = work.remaining();
        let totals = [
            source_initial - budget.source_bytes_remaining(),
            steps_initial - budget.steps_remaining(),
            original.io_bytes - final_work.io_bytes,
            original.records - final_work.records,
            original.output_bytes - final_work.output_bytes,
        ];
        const SOURCE: usize = 0;
        const STEPS: usize = 1;
        const IO: usize = 2;
        const RECORDS: usize = 3;
        const OUTPUT: usize = 4;
        for (kind, total) in totals.into_iter().enumerate() {
            assert!(total > 0);
            for limit in 0..total {
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                match kind {
                    SOURCE | STEPS => {
                        let bytes = if kind == SOURCE {
                            budget.source_bytes_remaining() - limit
                        } else {
                            0
                        };
                        let steps = if kind == STEPS {
                            budget.steps_remaining() - limit
                        } else {
                            0
                        };
                        let mut prep = Meter::new(
                            Deadline::after(Tick(0), 100).unwrap(),
                            Charge {
                                io_bytes: 100_000_000,
                                records: 100_000_000,
                                ..Charge::default()
                            },
                        );
                        budget
                            .charge(&mut prep, Tick(1), bytes, steps, &mut 0)
                            .unwrap();
                    }
                    IO | RECORDS | OUTPUT => {
                        let mut charge = work.remaining();
                        match kind {
                            IO => charge.io_bytes = limit,
                            RECORDS => charge.records = limit,
                            OUTPUT => charge.output_bytes = limit,
                            _ => panic!("unknown cut"),
                        }
                        work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), charge);
                    }
                    _ => panic!("unknown cut"),
                }
                let error = Error::Parameter(match kind {
                    SOURCE | STEPS => ParameterError::InterpretationLimit,
                    IO => ParameterError::Work(Stop::IoBytes),
                    RECORDS => ParameterError::Work(Stop::Records),
                    OUTPUT => ParameterError::Work(Stop::OutputBytes),
                    _ => panic!("unknown cut"),
                });
                let mut output = [0; 70];
                {
                    let mut cursor =
                        Cursor::new(input, Purpose::Charset, &mut output, &mut work, &mut budget);
                    assert_eq!(drain(&mut cursor), Err(error));
                    assert!(cursor.value().is_none());
                    let remaining = cursor.work.remaining();
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                    assert_eq!(cursor.work.remaining(), remaining);
                    assert!(cursor.finish(Tick(1)).is_err());
                }
                let mut independent = meter();
                let admission = budget.charge(&mut independent, Tick(1), 0, 0, &mut 0);
                if kind < IO {
                    assert_eq!(admission, Err(crate::nfc::Error::InterpretationLimit));
                } else {
                    assert_eq!(admission, Ok(()));
                }
            }
        }
        for turn in 0..=turns {
            for via_poll in [false, true] {
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                let mut output = [0; 70];
                let mut cursor =
                    Cursor::new(input, Purpose::Charset, &mut output, &mut work, &mut budget);
                for _ in 0..turn {
                    cursor.poll(Tick(1)).unwrap();
                }
                let error = Error::Parameter(ParameterError::Work(Stop::Deadline));
                if via_poll && turn < turns {
                    assert_eq!(cursor.poll(Tick(100)), Err(error));
                } else {
                    assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
                }
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert!(cursor.finish(Tick(1)).is_err());
            }
        }
    }
    #[test]
    fn invalid_grammar_wins_over_capacity_on_either_side_of_window() {
        for length in 0..=20 {
            let source = format!("text/plain;charset=\"{} x\"", "a".repeat(length));
            for capacity in 0..=8 {
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                let mut output = [0; 8];
                let mut cursor = Cursor::new(
                    source.as_bytes(),
                    Purpose::Charset,
                    output.get_mut(..capacity).unwrap(),
                    &mut work,
                    &mut budget,
                );
                assert_eq!(drain(&mut cursor).unwrap().value, Value::Invalid);
                assert!(cursor.value().is_none());
            }
        }
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut output = [];
        let mut cursor = Cursor::new(
            b"text/plain;charset=ok;broken",
            Purpose::Charset,
            &mut output,
            &mut work,
            &mut budget,
        );
        assert_eq!(
            drain(&mut cursor),
            Err(Error::Parameter(ParameterError::Malformed))
        );
    }
    #[test]
    fn capacity_source_and_fresh_deadline_refuse_without_fallback() {
        for capacity in 0..3 {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut output = [0; 3];
            let mut cursor = Cursor::new(
                b"multipart/mixed;boundary=f;boundary*=utf-8''ABC",
                Purpose::Boundary,
                output.get_mut(..capacity).unwrap(),
                &mut work,
                &mut budget,
            );
            assert_eq!(drain(&mut cursor), Err(Error::OutputCapacity));
            assert!(cursor.value().is_none());
            assert_eq!(cursor.check_deadline(Tick(1)), Err(Error::OutputCapacity));
            assert!(cursor.finish(Tick(1)).is_err());
        }
        for source in [
            b"multipart/mixed;boundary=ok;broken".as_slice(),
            b"multipart/mixed;boundary=\"a\xff\"",
            b"multipart/mixed;boundary=ok",
        ] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut output = [0; 70];
            let mut cursor = Cursor::new(
                source,
                Purpose::Boundary,
                &mut output,
                &mut work,
                &mut budget,
            );
            if source.ends_with(b"broken") || source.contains(&0xff) {
                assert!(matches!(
                    drain(&mut cursor),
                    Err(Error::Parameter(ParameterError::Malformed))
                ));
                assert!(cursor.value().is_none());
            } else {
                drain(&mut cursor).unwrap();
                assert_eq!(
                    cursor.check_deadline(Tick(100)),
                    Err(Error::Parameter(ParameterError::Work(
                        crate::admission::work::Stop::Deadline
                    )))
                );
                assert!(cursor.value().is_none());
            }
        }
    }
}
