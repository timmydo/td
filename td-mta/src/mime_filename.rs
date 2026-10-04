//! Retained display name from first-valid MIME fields; field authority is external.
use crate::{
    admission::work::Meter,
    mime_fields::Kind,
    mime_parameter::{display::normalized, Attribute},
    nfc::{self, HeaderBudget, Scratch},
    ports::Tick,
};

/// Conservative retained UTF-8 capacity for one raw selected field value.
/// Overflow means the bound is not representable. See RESOURCES.md.
pub const fn capacity_bound(field_bytes: usize) -> Option<usize> {
    field_bytes.checked_mul(16)
}

/// Exact values selected by MIME metadata; no later duplicate is supplied here.
#[derive(Clone, Copy)]
pub struct Fields<'a> {
    pub disposition: Option<&'a [u8]>,
    pub content_type: Option<&'a [u8]>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Origin {
    Disposition,
    ContentType,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct End {
    pub origin: Option<Origin>,
    pub bytes: usize,
    pub invalid_extended: bool,
    pub is_encoding_problem: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete(End),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Decode(normalized::Error),
    Admission(nfc::Error),
    OutputCapacity,
    InvalidState,
}
impl From<normalized::Error> for Error {
    fn from(error: normalized::Error) -> Self {
        match error {
            normalized::Error::Parameter(crate::mime_parameter::Error::Work(stop))
            | normalized::Error::Normalization(nfc::Error::Work(stop)) => {
                Self::Admission(nfc::Error::Work(stop))
            }
            normalized::Error::Parameter(crate::mime_parameter::Error::InterpretationLimit)
            | normalized::Error::Normalization(nfc::Error::InterpretationLimit) => {
                Self::Admission(nfc::Error::InterpretationLimit)
            }
            other => Self::Decode(other),
        }
    }
}
/// Passive validated UTF-8 bytes, bound to caller backing; publication needs
/// the enclosing owner's fresh original admission after all metadata succeeds.
pub struct Retained<'w> {
    pub end: End,
    value: Option<&'w [u8]>,
}
impl<'w> Retained<'w> {
    pub const fn value(&self) -> Option<&'w [u8]> {
        self.value
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decode(error) => write!(f, "MIME filename: {error}"),
            Self::Admission(error) => write!(f, "MIME filename admission: {error}"),
            Self::OutputCapacity => f.write_str("MIME filename output capacity"),
            Self::InvalidState => f.write_str("invalid MIME filename state"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Decode(error) => Some(error),
            Self::Admission(error) => Some(error),
            _ => None,
        }
    }
}
// One inline source shares the existing parser reservation.
#[allow(clippy::large_enum_variant)]
enum Owner<'a, 'w> {
    Budgets(&'w mut Meter, &'w mut HeaderBudget, &'w mut Scratch),
    Value(normalized::Cursor<'a, 'w>),
    Retired,
}
/// Retains UTF-8 in caller-reserved backing. No bytes are visible before
/// completion; they remain provisional at the enclosing metadata/job boundary.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_filename::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_filename::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    fields: Fields<'a>,
    output: &'w mut [u8],
    owner: Owner<'a, 'w>,
    origin: Origin,
    used: usize,
    invalid_extended: bool,
    end: Option<End>,
    failure: Option<Error>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(
        fields: Fields<'a>,
        output: &'w mut [u8],
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
        scratch: &'w mut Scratch,
    ) -> Self {
        Self {
            fields,
            output,
            owner: Owner::Budgets(work, budget, scratch),
            origin: Origin::Disposition,
            used: 0,
            invalid_extended: false,
            end: None,
            failure: None,
        }
    }
    /// A successful projection is not a path or authorization to publish.
    pub fn value(&self) -> Option<&[u8]> {
        let end = self.end?;
        if self.failure.is_some() || end.origin.is_none() {
            return None;
        }
        self.output.get(..end.bytes)
    }
    fn admission(&mut self, now: Tick) -> Result<(), Error> {
        match &mut self.owner {
            Owner::Budgets(work, budget, _) => budget
                .charge(work, now, 0, 0, &mut 0)
                .map_err(Error::Admission),
            Owner::Value(value) => value.check_deadline(now).map_err(Error::from),
            Owner::Retired => Err(Error::InvalidState),
        }
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
            self.end = None;
            self.used = 0;
            self.owner = Owner::Retired;
        }
        result
    }
    /// Hand off only healthy completion, with fresh admission and original
    /// budgets/scratch. Retained bytes remain provisional at the job boundary.
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<
        (
            Retained<'w>,
            &'w mut Meter,
            &'w mut HeaderBudget,
            &'w mut Scratch,
        ),
        Error,
    > {
        self.check_deadline(now)?;
        let end = self.end.ok_or(Error::InvalidState)?;
        let Owner::Budgets(work, budget, scratch) = self.owner else {
            return Err(Error::InvalidState);
        };
        let output: &'w [u8] = self.output;
        let value = if end.origin.is_some() {
            Some(output.get(..end.bytes).ok_or(Error::InvalidState)?)
        } else {
            None
        };
        Ok((Retained { end, value }, work, budget, scratch))
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.admission(now);
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
        match &mut self.owner {
            Owner::Budgets(work, budget, _) => {
                budget
                    .charge(work, now, 0, 1, &mut 0)
                    .map_err(Error::Admission)?;
                let (source, kind, attribute) = match self.origin {
                    Origin::Disposition => (
                        self.fields.disposition,
                        Kind::ContentDisposition,
                        Attribute::Filename,
                    ),
                    Origin::ContentType => {
                        (self.fields.content_type, Kind::ContentType, Attribute::Name)
                    }
                };
                let Some(source) = source else {
                    return self.next_or_complete(false, false);
                };
                let Owner::Budgets(work, budget, scratch) =
                    std::mem::replace(&mut self.owner, Owner::Retired)
                else {
                    return Err(Error::InvalidState);
                };
                self.owner = Owner::Value(
                    normalized::Cursor::new(source, kind, attribute, scratch, work, budget)
                        .map_err(Error::from)?,
                );
                Ok(Status::Yield)
            }
            Owner::Value(value) => match value.poll(now).map_err(Error::from)? {
                normalized::Status::Yield => Ok(Status::Yield),
                normalized::Status::Scalar(scalar) => {
                    let mut bytes = [0; 4];
                    let bytes = scalar.encode_utf8(&mut bytes).as_bytes();
                    let next = self
                        .used
                        .checked_add(bytes.len())
                        .ok_or(Error::OutputCapacity)?;
                    let target = self
                        .output
                        .get_mut(self.used..next)
                        .ok_or(Error::OutputCapacity)?;
                    value
                        .charge_output(now, bytes.len() as u64)
                        .map_err(Error::from)?;
                    target.copy_from_slice(bytes);
                    self.used = next;
                    Ok(Status::Yield)
                }
                normalized::Status::Complete(decoded) => {
                    let Owner::Value(value) = std::mem::replace(&mut self.owner, Owner::Retired)
                    else {
                        return Err(Error::InvalidState);
                    };
                    let (work, budget, scratch) = value.finish(now).map_err(Error::from)?;
                    self.owner = Owner::Budgets(work, budget, scratch);
                    self.invalid_extended |= decoded.selection.invalid_extended;
                    self.next_or_complete(
                        decoded.selection.plan.is_some(),
                        decoded.is_encoding_problem,
                    )
                }
            },
            Owner::Retired => Err(Error::InvalidState),
        }
    }
    fn next_or_complete(&mut self, found: bool, problem: bool) -> Result<Status, Error> {
        if !found && self.used != 0 {
            return Err(Error::InvalidState);
        }
        if !found && self.origin == Origin::Disposition {
            self.origin = Origin::ContentType;
            return Ok(Status::Yield);
        }
        let end = End {
            origin: found.then_some(self.origin),
            bytes: self.used,
            invalid_extended: self.invalid_extended,
            is_encoding_problem: problem,
        };
        self.end = Some(end);
        Ok(Status::Complete(end))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{
        admission::work::{Charge, Stop},
        ports::Deadline,
    };
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
        panic!("filename did not finish")
    }
    #[test]
    fn precedence_empty_presence_normalization_and_diagnostics() {
        assert_eq!(capacity_bound(0), Some(0));
        assert_eq!(capacity_bound(usize::MAX / 16), Some(usize::MAX / 16 * 16));
        assert_eq!(capacity_bound(usize::MAX / 16 + 1), None);
        for (disposition, content_type, expected, origin, rejected, problem) in [
            (
                Some(b"attachment;filename=ordinary;filename*=utf-8''e%CC%81".as_slice()),
                Some(b"text/plain;name=other".as_slice()),
                Some("é"),
                Some(Origin::Disposition),
                false,
                false,
            ),
            (
                Some(b"attachment;filename=first;filename=second;filename*=utf-8''%xx".as_slice()),
                Some(b"text/plain;name=other".as_slice()),
                Some("first"),
                Some(Origin::Disposition),
                true,
                false,
            ),
            (
                Some(b"attachment;filename*=utf-8''%xx".as_slice()),
                Some(b"text/plain;name=ordinary;name*=utf-8''e%CC%81".as_slice()),
                Some("é"),
                Some(Origin::ContentType),
                true,
                false,
            ),
            (
                None,
                Some(b"text/plain;name=\"=?utf-8?Q?e?= =?utf-8?Q?=CC=81?=\"".as_slice()),
                Some("é"),
                Some(Origin::ContentType),
                false,
                false,
            ),
            (
                Some(b"attachment;filename=\"\"".as_slice()),
                Some(b"text/plain;name=other".as_slice()),
                Some(""),
                Some(Origin::Disposition),
                false,
                false,
            ),
            (
                Some(b"attachment;filename*=utf-8''%00".as_slice()),
                Some(b"text/plain;name=other".as_slice()),
                Some(""),
                Some(Origin::Disposition),
                false,
                false,
            ),
            (
                Some(b"attachment;filename*=utf-8''%FF".as_slice()),
                None,
                Some("�"),
                Some(Origin::Disposition),
                false,
                true,
            ),
            (
                Some(b"attachment;x=missing".as_slice()),
                Some(b"text/plain;x=missing".as_slice()),
                None,
                None,
                false,
                false,
            ),
            (None, None, None, None, false, false),
            (
                Some(b"attachment;x=missing".as_slice()),
                Some(b"text/plain;name=x".as_slice()),
                Some("x"),
                Some(Origin::ContentType),
                false,
                false,
            ),
            (
                None,
                Some(b"text/plain;name=\"\"".as_slice()),
                Some(""),
                Some(Origin::ContentType),
                false,
                false,
            ),
            (
                Some(b"attachment;filename*1*=%81;filename*0*=utf-8'en'e%CC".as_slice()),
                None,
                Some("é"),
                Some(Origin::Disposition),
                false,
                false,
            ),
            (
                Some(b"attachment;filename=first;filename*0*=utf-8''e;filename*2*=x".as_slice()),
                None,
                Some("first"),
                Some(Origin::Disposition),
                true,
                false,
            ),
        ] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut output = [0; 32];
            let mut cursor = Cursor::new(
                Fields {
                    disposition,
                    content_type,
                },
                &mut output,
                &mut work,
                &mut budget,
                &mut scratch,
            );
            assert!(std::mem::size_of_val(&cursor) + std::mem::size_of::<HeaderBudget>() <= 4800);
            assert_eq!(cursor.value(), None);
            let end = drain(&mut cursor).unwrap();
            assert_eq!(end.origin, origin);
            assert_eq!(end.invalid_extended, rejected);
            assert_eq!(end.is_encoding_problem, problem);
            assert_eq!(cursor.value(), expected.map(str::as_bytes));
            let raw = match origin {
                Some(Origin::Disposition) => disposition.unwrap().len(),
                Some(Origin::ContentType) => content_type.unwrap().len(),
                None => 0,
            };
            assert!(expected.map_or(0, str::len) <= capacity_bound(raw).unwrap());
            assert_eq!(end.bytes, expected.map_or(0, str::len));
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete(end)));
            cursor.check_deadline(Tick(1)).unwrap();
            let error = Error::Admission(nfc::Error::Work(Stop::Deadline));
            assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
            assert_eq!(cursor.value(), None);
            assert_eq!(cursor.poll(Tick(1)), Err(error));
            assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
        }
    }
    #[test]
    fn capacity_and_malformed_source_never_fall_back() {
        for capacity in 0..4 {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut output = [0; 3];
            let mut cursor = Cursor::new(
                Fields {
                    disposition: Some(b"attachment;filename*=utf-8''e%CC%81x"),
                    content_type: Some(b"text/plain;name=f"),
                },
                output.get_mut(..capacity).unwrap(),
                &mut work,
                &mut budget,
                &mut scratch,
            );
            let result = drain(&mut cursor);
            if capacity == 3 {
                assert!(result.is_ok());
                assert_eq!(cursor.value(), Some("éx".as_bytes()));
            } else {
                assert_eq!(result, Err(Error::OutputCapacity));
                assert_eq!(cursor.value(), None);
                assert_eq!(cursor.poll(Tick(1)), Err(Error::OutputCapacity));
            }
        }
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut output = [0; 32];
        let mut cursor = Cursor::new(
            Fields {
                disposition: Some(b"attachment;filename=first;"),
                content_type: Some(b"text/plain;name=f"),
            },
            &mut output,
            &mut work,
            &mut budget,
            &mut scratch,
        );
        let error = Error::Decode(normalized::Error::Parameter(
            crate::mime_parameter::Error::Malformed,
        ));
        assert_eq!(drain(&mut cursor), Err(error));
        assert_eq!(cursor.value(), None);
        assert_eq!(cursor.poll(Tick(1)), Err(error));
    }
    #[test]
    fn every_original_allowance_cut_retires_retained_value() {
        const SOURCE_BYTES: usize = 0;
        const STEPS: usize = 1;
        const JOB_IO: usize = 2;
        const JOB_RECORDS: usize = 3;
        const OUTPUT: usize = 4;
        let fields = Fields {
            disposition: Some(b"attachment;filename*=utf-8''%xx"),
            content_type: Some(b"text/plain;name*=utf-8''e%CC%81x"),
        };
        let mut work = meter();
        let initial = work.remaining();
        let mut budget = HeaderBudget::new();
        let before_bytes = budget.source_bytes_remaining();
        let before_steps = budget.steps_remaining();
        let mut scratch = Scratch::new();
        let mut output = [0; 16];
        {
            let mut cursor = Cursor::new(fields, &mut output, &mut work, &mut budget, &mut scratch);
            assert_eq!(
                drain(&mut cursor).unwrap().origin,
                Some(Origin::ContentType)
            );
        }
        let used = [
            before_bytes - budget.source_bytes_remaining(),
            before_steps - budget.steps_remaining(),
            initial.io_bytes - work.remaining().io_bytes,
            initial.records - work.remaining().records,
            initial.output_bytes - work.remaining().output_bytes,
        ];
        for (flavor, total) in used.into_iter().enumerate() {
            assert!(total > 0);
            for limit in 0..total {
                let mut charge = Charge {
                    io_bytes: 100_000_000,
                    records: 100_000_000,
                    output_bytes: 100_000_000,
                    ..Charge::default()
                };
                match flavor {
                    JOB_IO => charge.io_bytes = limit,
                    JOB_RECORDS => charge.records = limit,
                    OUTPUT => charge.output_bytes = limit,
                    _ => {}
                }
                let mut work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), charge);
                let mut budget = HeaderBudget::new();
                if flavor < JOB_IO {
                    let bytes = if flavor == SOURCE_BYTES {
                        budget.source_bytes_remaining() - limit
                    } else {
                        0
                    };
                    let steps = if flavor == STEPS {
                        budget.steps_remaining() - limit
                    } else {
                        0
                    };
                    budget
                        .charge(&mut work, Tick(1), bytes, steps, &mut 0)
                        .unwrap();
                }
                let mut scratch = Scratch::new();
                let mut output = [0; 16];
                {
                    let mut cursor =
                        Cursor::new(fields, &mut output, &mut work, &mut budget, &mut scratch);
                    let error = drain(&mut cursor).err().unwrap();
                    let expected = match flavor {
                        SOURCE_BYTES | STEPS => nfc::Error::InterpretationLimit,
                        JOB_IO => nfc::Error::Work(Stop::IoBytes),
                        JOB_RECORDS => nfc::Error::Work(Stop::Records),
                        OUTPUT => nfc::Error::Work(Stop::OutputBytes),
                        other => panic!("unexpected allowance {other}"),
                    };
                    assert_eq!(error, Error::Admission(expected));
                    assert_eq!(cursor.value(), None);
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                }
                let remaining = work.remaining();
                let bytes = budget.source_bytes_remaining();
                let steps = budget.steps_remaining();
                if flavor < JOB_IO {
                    assert_eq!(
                        budget.charge(&mut work, Tick(1), 0, 0, &mut 0),
                        Err(nfc::Error::InterpretationLimit)
                    );
                    assert_eq!(work.remaining(), remaining);
                } else {
                    let mut independent_job = meter();
                    budget
                        .charge(&mut independent_job, Tick(1), 0, 0, &mut 0)
                        .unwrap();
                }
                assert_eq!(
                    (budget.source_bytes_remaining(), budget.steps_remaining()),
                    (bytes, steps)
                );
            }
        }
    }
    #[test]
    fn healthy_handoff_keeps_bytes_and_original_owners_for_later_metadata() {
        for expected in [Some("é"), Some(""), None] {
            let fields = Fields {
                disposition: expected.map(|name| {
                    if name.is_empty() {
                        b"attachment;filename=\"\"".as_slice()
                    } else {
                        b"attachment;filename*=utf-8''e%CC%81".as_slice()
                    }
                }),
                content_type: None,
            };
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut output = [0; 16];
            let identities = (
                std::ptr::from_ref(&work),
                std::ptr::from_ref(&budget),
                std::ptr::from_ref(&scratch),
            );
            let mut cursor = Cursor::new(fields, &mut output, &mut work, &mut budget, &mut scratch);
            let end = drain(&mut cursor).unwrap();
            let (retained, work, budget, scratch) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(retained.end, end);
            assert_eq!(retained.value(), expected.map(str::as_bytes));
            assert_eq!(
                (
                    std::ptr::from_ref(work),
                    std::ptr::from_ref(budget),
                    std::ptr::from_ref(scratch)
                ),
                identities
            );
            let mut next = crate::mime_parameter::BudgetedOctets::new(
                b"text/plain;charset=utf-8",
                Kind::ContentType,
                Attribute::Charset,
                work,
                budget,
            );
            let mut done = false;
            for _ in 0..1000 {
                if matches!(
                    next.poll(Tick(1)).unwrap(),
                    crate::mime_parameter::OctetStatus::Complete(_)
                ) {
                    done = true;
                    break;
                }
            }
            assert!(done);
            assert_eq!(retained.value(), expected.map(str::as_bytes));
        }
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut output = [];
        let cursor = Cursor::new(
            Fields {
                disposition: None,
                content_type: None,
            },
            &mut output,
            &mut work,
            &mut budget,
            &mut scratch,
        );
        assert!(matches!(cursor.finish(Tick(1)), Err(Error::InvalidState)));
    }
    #[test]
    fn every_live_deadline_cut_and_failed_handoff_hide_bytes() {
        let fields = Fields {
            disposition: Some(b"attachment;filename*=utf-8''e%CC%81x"),
            content_type: None,
        };
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut output = [0; 16];
        let turns = {
            let mut cursor = Cursor::new(fields, &mut output, &mut work, &mut budget, &mut scratch);
            let mut turns = 0;
            loop {
                turns += 1;
                if matches!(cursor.poll(Tick(1)).unwrap(), Status::Complete(_)) {
                    break;
                }
            }
            turns
        };
        for turn in 0..=turns {
            for via_poll in [false, true] {
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                let mut scratch = Scratch::new();
                let mut output = [0; 16];
                let mut cursor =
                    Cursor::new(fields, &mut output, &mut work, &mut budget, &mut scratch);
                for _ in 0..turn {
                    cursor.poll(Tick(1)).unwrap();
                }
                let error = Error::Admission(nfc::Error::Work(Stop::Deadline));
                if via_poll && turn < turns {
                    assert_eq!(cursor.poll(Tick(100)), Err(error));
                } else {
                    assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
                }
                assert_eq!(cursor.value(), None);
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert!(matches!(cursor.finish(Tick(1)),Err(e) if e==error));
            }
        }
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut output = [0; 16];
        let mut cursor = Cursor::new(fields, &mut output, &mut work, &mut budget, &mut scratch);
        drain(&mut cursor).unwrap();
        assert!(matches!(
            cursor.finish(Tick(100)),
            Err(Error::Admission(nfc::Error::Work(Stop::Deadline)))
        ));
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut output = [];
        let mut cursor = Cursor::new(fields, &mut output, &mut work, &mut budget, &mut scratch);
        assert_eq!(drain(&mut cursor), Err(Error::OutputCapacity));
        assert!(matches!(cursor.finish(Tick(1)), Err(Error::OutputCapacity)));
        let nested = format!("{}x{} text/plain;name=x", "(".repeat(33), ")".repeat(33));
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut output = [0; 16];
        let mut cursor = Cursor::new(
            Fields {
                disposition: None,
                content_type: Some(nested.as_bytes()),
            },
            &mut output,
            &mut work,
            &mut budget,
            &mut scratch,
        );
        assert_eq!(
            drain(&mut cursor),
            Err(Error::Decode(normalized::Error::Parameter(
                crate::mime_parameter::Error::NestingLimit
            )))
        );
    }
}
