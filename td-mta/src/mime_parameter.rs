//! Complete candidate validation through fixed-state charged field replay.
use crate::{
    admission::work::{Charge, Meter, Stop},
    decode_work::{self, Work},
    mime_attribute::{self, Form},
    mime_fields::{self, Parameter},
    mime_value,
    ports::Tick,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Attribute {
    Boundary,
    Charset,
    Name,
    Filename,
}
impl Attribute {
    const fn bytes(self) -> &'static [u8] {
        match self {
            Self::Boundary => b"boundary",
            Self::Charset => b"charset",
            Self::Name => b"name",
            Self::Filename => b"filename",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Plan {
    Ordinary(Parameter),
    Extended(Parameter),
    Sections { count: u64, initial_encoded: bool },
}
/// Candidate evidence and a passive rejection diagnostic, not metadata authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    pub plan: Option<Plan>,
    pub invalid_extended: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete(Selection),
}
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
        match self {
            Self::Malformed => f.write_str("malformed MIME parameter candidate"),
            Self::NestingLimit => f.write_str("MIME parameter nesting limit"),
            Self::Work(e) => write!(f, "MIME parameter work: {e}"),
            Self::InterpretationLimit => f.write_str("MIME parameter interpretation limit"),
            Self::InvalidState => f.write_str("invalid MIME parameter state"),
        }
    }
}
impl std::error::Error for Error {}
impl From<decode_work::Error> for Error {
    fn from(e: decode_work::Error) -> Self {
        match e {
            decode_work::Error::Work(e) => Self::Work(e),
            decode_work::Error::InterpretationLimit => Self::InterpretationLimit,
            decode_work::Error::InvalidState => Self::InvalidState,
        }
    }
}
impl From<mime_fields::Error> for Error {
    fn from(e: mime_fields::Error) -> Self {
        match e {
            mime_fields::Error::Malformed => Self::Malformed,
            mime_fields::Error::NestingLimit => Self::NestingLimit,
            mime_fields::Error::Work(e) => Self::Work(e),
            mime_fields::Error::InterpretationLimit => Self::InterpretationLimit,
            mime_fields::Error::InvalidState => Self::InvalidState,
        }
    }
}
impl From<mime_attribute::Error> for Error {
    fn from(e: mime_attribute::Error) -> Self {
        match e {
            mime_attribute::Error::Work(e) => Self::Work(e),
            mime_attribute::Error::InterpretationLimit => Self::InterpretationLimit,
            mime_attribute::Error::InvalidState => Self::InvalidState,
        }
    }
}
impl From<mime_value::Error> for Error {
    fn from(e: mime_value::Error) -> Self {
        match e {
            mime_value::Error::Malformed => Self::Malformed,
            mime_value::Error::Work(e) => Self::Work(e),
            mime_value::Error::InterpretationLimit => Self::InterpretationLimit,
            mime_value::Error::InvalidState => Self::InvalidState,
        }
    }
}
#[derive(Clone, Copy)]
enum Family {
    None,
    Single(Parameter),
    Sections,
    Invalid,
}
#[derive(Clone, Copy)]
enum Pass {
    Initial,
    Lookup(u64),
}
#[derive(Clone, Copy)]
enum Phase {
    Start,
    Scan,
    Name,
    Match,
    Validate,
    Finish,
    Complete,
}
/// Complete candidate selection; source plans are not publication authority.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_parameter::Cursor<'_>>();
/// ```
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_parameter::Cursor<'_>>();
/// ```
pub struct Cursor<'a> {
    source: &'a [u8],
    kind: mime_fields::Kind,
    attribute: Attribute,
    phase: Phase,
    pass: Pass,
    fields: Option<mime_fields::Cursor<'a>>,
    name: Option<mime_attribute::Cursor<'a>>,
    value: Option<mime_value::Cursor<'a>>,
    parameter: Option<Parameter>,
    classified: Option<mime_attribute::Name>,
    ordinary: Option<Parameter>,
    family: Family,
    count: u64,
    max: u64,
    found: Option<(Parameter, bool)>,
    duplicate: bool,
    initial_encoded: bool,
    result: Selection,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    #[must_use]
    pub const fn new(source: &'a [u8], kind: mime_fields::Kind, attribute: Attribute) -> Self {
        Self {
            source,
            kind,
            attribute,
            phase: Phase::Start,
            pass: Pass::Initial,
            fields: None,
            name: None,
            value: None,
            parameter: None,
            classified: None,
            ordinary: None,
            family: Family::None,
            count: 0,
            max: 0,
            found: None,
            duplicate: false,
            initial_encoded: false,
            result: Selection {
                plan: None,
                invalid_extended: false,
            },
            failure: None,
        }
    }
    pub fn check_deadline(&mut self, now: Tick, work: &mut Meter) -> Result<(), Error> {
        self.check(|| Work::charge(work, now, Charge::default()))
    }
    fn check(
        &mut self,
        admit: impl FnOnce() -> Result<(), decode_work::Error>,
    ) -> Result<(), Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let result = admit().map_err(Error::from);
        if let Err(e) = result {
            self.failure = Some(e);
            self.result.plan = None;
        }
        result
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        self.poll_with_work(now, work)
    }
    fn poll_with_work(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete(self.result));
        }
        let result = self.step(now, work);
        if let Err(e) = result {
            self.failure = Some(e);
            self.result.plan = None;
        }
        result
    }
    fn begin_value(&mut self, p: Parameter, mode: mime_value::Mode) -> Result<(), Error> {
        let source = self
            .source
            .get(p.value.start..p.value.end)
            .ok_or(Error::InvalidState)?;
        self.value = Some(mime_value::Cursor::new(source, p.quoted, mode));
        self.phase = Phase::Validate;
        Ok(())
    }
    fn choose_fallback(&mut self) {
        self.result.invalid_extended = !matches!(self.family, Family::None);
        self.family = Family::Invalid;
        self.value = None;
        self.result.plan = self.ordinary.map(Plan::Ordinary);
        self.phase = Phase::Finish;
    }
    fn step(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
        work.charge(
            now,
            Charge {
                records: 1,
                ..Charge::default()
            },
        )?;
        match self.phase {
            Phase::Start => {
                if self.kind == mime_fields::Kind::TransferEncoding {
                    return Err(Error::InvalidState);
                }
                self.fields = Some(mime_fields::Cursor::new(self.source, self.kind));
                self.found = None;
                self.duplicate = false;
                self.phase = Phase::Scan;
            }
            Phase::Scan => {
                match self
                    .fields
                    .as_mut()
                    .ok_or(Error::InvalidState)?
                    .poll_with_work(now, work)?
                {
                    mime_fields::Status::Yield | mime_fields::Status::Head(_) => {}
                    mime_fields::Status::Parameter(p) => {
                        let source = self
                            .source
                            .get(p.name.start..p.name.end)
                            .ok_or(Error::InvalidState)?;
                        self.parameter = Some(p);
                        self.name = Some(mime_attribute::Cursor::new(source));
                        self.phase = Phase::Name;
                    }
                    mime_fields::Status::Complete => {
                        self.fields = None;
                        match self.pass {
                            Pass::Initial => match self.family {
                                Family::Single(p) => {
                                    self.begin_value(p, mime_value::Mode::ExtendedInitial)?
                                }
                                Family::Sections => {
                                    if self.max.checked_add(1) != Some(self.count) {
                                        self.choose_fallback()
                                    } else {
                                        self.pass = Pass::Lookup(0);
                                        self.phase = Phase::Start;
                                    }
                                }
                                _ => self.choose_fallback(),
                            },
                            Pass::Lookup(index) => {
                                if self.duplicate {
                                    self.choose_fallback()
                                } else if let Some((p, encoded)) = self.found {
                                    if index == 0 {
                                        self.initial_encoded = encoded;
                                    }
                                    let mode = if !encoded {
                                        mime_value::Mode::Ordinary
                                    } else if index == 0 {
                                        mime_value::Mode::ExtendedInitial
                                    } else {
                                        mime_value::Mode::ExtendedContinuation
                                    };
                                    self.begin_value(p, mode)?;
                                } else {
                                    self.choose_fallback()
                                }
                            }
                        }
                    }
                }
            }
            Phase::Name => {
                if let mime_attribute::Status::Complete(name) = self
                    .name
                    .as_mut()
                    .ok_or(Error::InvalidState)?
                    .poll_with_work(now, work)?
                {
                    self.classified = Some(name);
                    self.name = None;
                    self.phase = Phase::Match;
                }
            }
            Phase::Match => {
                let name = self.classified.take().ok_or(Error::InvalidState)?;
                let p = self.parameter.take().ok_or(Error::InvalidState)?;
                let length = name
                    .base
                    .end
                    .checked_sub(name.base.start)
                    .ok_or(Error::InvalidState)?;
                self.phase = Phase::Scan;
                if length != self.attribute.bytes().len() {
                    return Ok(Status::Yield);
                }
                let start = p
                    .name
                    .start
                    .checked_add(name.base.start)
                    .ok_or(Error::InvalidState)?;
                let end = start.checked_add(length).ok_or(Error::InvalidState)?;
                work.charge(
                    now,
                    Charge {
                        io_bytes: length as u64,
                        ..Charge::default()
                    },
                )?;
                if !self
                    .source
                    .get(start..end)
                    .ok_or(Error::InvalidState)?
                    .eq_ignore_ascii_case(self.attribute.bytes())
                {
                    return Ok(Status::Yield);
                }
                match self.pass {
                    Pass::Initial => match name.form {
                        Form::Ordinary => {
                            if self.ordinary.is_none() {
                                self.ordinary = Some(p)
                            }
                        }
                        Form::Extended => {
                            self.family = if matches!(self.family, Family::None) {
                                Family::Single(p)
                            } else {
                                Family::Invalid
                            }
                        }
                        Form::Malformed => self.family = Family::Invalid,
                        Form::Section { index, .. } => {
                            match self.family {
                                Family::None => self.family = Family::Sections,
                                Family::Single(_) => self.family = Family::Invalid,
                                _ => {}
                            }
                            if matches!(self.family, Family::Sections) {
                                self.count =
                                    self.count.checked_add(1).ok_or(Error::InvalidState)?;
                                self.max = self.max.max(index);
                            }
                        }
                    },
                    Pass::Lookup(wanted) => {
                        if let Form::Section { index, encoded } = name.form {
                            if index == wanted {
                                if self.found.is_some() {
                                    self.duplicate = true;
                                } else {
                                    self.found = Some((p, encoded));
                                }
                            }
                        }
                    }
                }
            }
            Phase::Validate => {
                match self
                    .value
                    .as_mut()
                    .ok_or(Error::InvalidState)?
                    .poll_with_work(now, work)
                {
                    Ok(mime_value::Status::Yield | mime_value::Status::Octet { .. }) => {}
                    Err(mime_value::Error::Malformed) => self.choose_fallback(),
                    Err(e) => return Err(e.into()),
                    Ok(mime_value::Status::Complete) => {
                        self.value = None;
                        match self.pass {
                            Pass::Initial => {
                                let Family::Single(p) = self.family else {
                                    return Err(Error::InvalidState);
                                };
                                self.result.plan = Some(Plan::Extended(p));
                                self.phase = Phase::Finish;
                            }
                            Pass::Lookup(index) => {
                                let next = index.checked_add(1).ok_or(Error::InvalidState)?;
                                if next == self.count {
                                    self.result.plan = Some(Plan::Sections {
                                        count: self.count,
                                        initial_encoded: self.initial_encoded,
                                    });
                                    self.phase = Phase::Finish;
                                } else {
                                    self.pass = Pass::Lookup(next);
                                    self.phase = Phase::Start;
                                }
                            }
                        }
                    }
                }
            }
            Phase::Finish => {
                self.phase = Phase::Complete;
                return Ok(Status::Complete(self.result));
            }
            Phase::Complete => return Err(Error::InvalidState),
        }
        Ok(Status::Yield)
    }
}

/// Retains the original job and header interpretation allowance across all passes.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_parameter::Budgeted<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_parameter::Budgeted<'_, '_>>();
/// ```
pub struct Budgeted<'a, 'w> {
    cursor: Cursor<'a>,
    work: &'w mut Meter,
    budget: &'w mut crate::nfc::HeaderBudget,
    credit: u8,
}
impl<'a, 'w> Budgeted<'a, 'w> {
    #[must_use]
    pub fn new(
        source: &'a [u8],
        kind: mime_fields::Kind,
        attribute: Attribute,
        work: &'w mut Meter,
        budget: &'w mut crate::nfc::HeaderBudget,
    ) -> Self {
        Self {
            cursor: Cursor::new(source, kind, attribute),
            work,
            budget,
            credit: 0,
        }
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.cursor.check(|| {
            td_header::Work::charge(
                &mut decode_work::Admission::new(now, self.work, self.budget, &mut self.credit),
                td_header::Charge::default(),
            )
        })
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        self.cursor.poll_with_work(
            now,
            &mut decode_work::Parsing::new(self.work, self.budget, &mut self.credit),
        )
    }
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
                io_bytes: 100_000_000,
                records: 100_000_000,
                ..Charge::default()
            },
        )
    }
    fn select(source: &[u8]) -> Result<Option<Plan>, Error> {
        let mut cursor = Cursor::new(
            source,
            mime_fields::Kind::ContentDisposition,
            Attribute::Filename,
        );
        assert!(std::mem::size_of_val(&cursor) <= 1024);
        let mut work = work();
        for _ in 0..10000 {
            let before = work.remaining();
            let status = cursor.poll(Tick(1), &mut work)?;
            assert!(before.io_bytes - work.remaining().io_bytes <= 160);
            assert!(before.records - work.remaining().records <= 33);
            if let Status::Complete(p) = status {
                let before = work.remaining();
                assert_eq!(cursor.poll(Tick(100), &mut work), Ok(Status::Complete(p)));
                assert_eq!(work.remaining(), before);
                return Ok(p.plan);
            }
        }
        panic!("not complete")
    }
    #[test]
    fn first_ordinary_and_single_extended_precedence() {
        for (source, raw, extended) in [
            (
                b"attachment;filename=one;filename=two".as_slice(),
                b"one".as_slice(),
                false,
            ),
            (
                b"attachment;filename=one;filename*=utf-8''two",
                b"utf-8''two",
                true,
            ),
            (
                b"attachment;filename*=utf-8''two;filename=one",
                b"utf-8''two",
                true,
            ),
            (
                b"attachment;filename*=utf-8''two%xx;filename=one",
                b"one",
                false,
            ),
            (
                b"attachment;filename*=utf-8''two;filename*=utf-8''three;filename=one",
                b"one",
                false,
            ),
        ] {
            let p = select(source).unwrap().unwrap();
            let p = match p {
                Plan::Ordinary(p) => {
                    assert!(!extended);
                    p
                }
                Plan::Extended(p) => {
                    assert!(extended);
                    p
                }
                _ => panic!("wrong plan"),
            };
            assert_eq!(source.get(p.value.start..p.value.end), Some(raw));
        }
        assert_eq!(select(b"attachment;x=one").unwrap(), None);
    }
    #[test]
    fn order_independent_mixed_sections_require_complete_series() {
        for source in [
            b"attachment;filename*2=last;filename*0*=utf-8''first;filename*1*=%20mid".as_slice(),
            b"attachment;filename*0=one;filename*1=two;filename*2=three",
            b"attachment;filename*0=one;filename*1*=%20two;filename*2=three",
        ] {
            assert!(matches!(
                select(source).unwrap(),
                Some(Plan::Sections { count: 3, .. })
            ))
        }
        for source in [
            b"attachment;filename*0*=utf-8''one;filename*2=two;filename=saved".as_slice(),
            b"attachment;filename*0=one;filename*0=dup;filename*2=two;filename=saved",
            b"attachment;filename*1=one;filename*1=dup;filename=saved",
            b"attachment;filename*0=one;filename*01=evil;filename*1=two;filename=saved",
            b"attachment;filename*0=one;filename*1=two;filename*=utf-8''other;filename=saved",
            b"attachment;filename*18446744073709551615=one;filename=saved",
            b"attachment;filename*0*=utf-8''%E2%;filename*1*=82;filename=saved",
        ] {
            let Some(Plan::Ordinary(p)) = select(source).unwrap() else {
                panic!("invalid series chosen")
            };
            assert_eq!(
                source.get(p.value.start..p.value.end),
                Some(b"saved".as_slice())
            );
        }
    }
    #[test]
    fn complete_rejection_diagnostic_and_final_admission_are_sticky() {
        for (source, rejected, has_plan) in [
            (b"attachment;x=one".as_slice(), false, false),
            (b"attachment;filename=one", false, true),
            (b"attachment;filename*01=bad", true, false),
            (
                b"attachment;filename*=utf-8''%xx;filename=saved",
                true,
                true,
            ),
            (
                b"attachment;filename*0=a;filename*2=b;filename=saved",
                true,
                true,
            ),
            (b"attachment;FILENAME*=utf-8'en'good", false, true),
        ] {
            let mut cursor = Cursor::new(
                source,
                mime_fields::Kind::ContentDisposition,
                Attribute::Filename,
            );
            let mut meter = work();
            while !matches!(cursor.phase, Phase::Finish) {
                assert_eq!(cursor.poll(Tick(1), &mut meter).unwrap(), Status::Yield);
            }
            assert_eq!(cursor.result.invalid_extended, rejected);
            assert_eq!(cursor.result.plan.is_some(), has_plan);
            assert_eq!(
                cursor.poll(Tick(100), &mut meter),
                Err(Error::Work(Stop::Deadline))
            );
            assert_eq!(cursor.result.plan, None);
            let mut replacement = work();
            let before = replacement.remaining();
            assert_eq!(
                cursor.poll(Tick(1), &mut replacement),
                Err(Error::Work(Stop::Deadline))
            );
            assert_eq!(
                cursor.check_deadline(Tick(1), &mut replacement),
                Err(Error::Work(Stop::Deadline))
            );
            assert_eq!(replacement.remaining(), before);
        }
        let mut cursor = Cursor::new(
            b"text/plain;charset=Us-Ascii",
            mime_fields::Kind::ContentType,
            Attribute::Charset,
        );
        let mut meter = work();
        let selected = loop {
            if let Status::Complete(selected) = cursor.poll(Tick(1), &mut meter).unwrap() {
                break selected;
            }
        };
        let Some(Plan::Ordinary(parameter)) = selected.plan else {
            panic!("charset not selected")
        };
        assert_eq!(
            b"text/plain;charset=Us-Ascii".get(parameter.value.start..parameter.value.end),
            Some(b"Us-Ascii".as_slice())
        );
        assert!(!selected.invalid_extended);
        assert_eq!(
            cursor.check_deadline(Tick(100), &mut meter),
            Err(Error::Work(Stop::Deadline))
        );
        assert_eq!(
            cursor.poll(Tick(1), &mut work()),
            Err(Error::Work(Stop::Deadline))
        );
        assert_eq!(
            Cursor::new(
                b"base64",
                mime_fields::Kind::TransferEncoding,
                Attribute::Filename
            )
            .poll(Tick(1), &mut work()),
            Err(Error::InvalidState)
        );
    }
    #[test]
    fn malformed_whole_field_and_depth_are_not_fallback() {
        assert_eq!(select(b"attachment;filename=saved;"), Err(Error::Malformed));
        let source = format!(
            "attachment;filename=saved {}x{}",
            "(".repeat(33),
            ")".repeat(33)
        );
        assert_eq!(select(source.as_bytes()), Err(Error::NestingLimit));
    }
    #[test]
    fn long_names_and_many_sections_are_bounded_charged_replay() {
        let mut source = String::from("attachment;filename=saved;");
        source.push_str(&"x".repeat(4096));
        source.push_str("=ignored");
        for index in (0..24).rev() {
            source.push_str(&format!(";filename*{index}=value"));
        }
        let mut meter = work();
        let mut budget = crate::nfc::HeaderBudget::new();
        let mut cursor = Budgeted::new(
            source.as_bytes(),
            mime_fields::Kind::ContentDisposition,
            Attribute::Filename,
            &mut meter,
            &mut budget,
        );
        let mut turns = 0;
        loop {
            let before = (
                cursor.work.remaining(),
                cursor.budget.source_bytes_remaining(),
                cursor.budget.steps_remaining(),
            );
            let status = cursor.poll(Tick(1)).unwrap();
            assert!(before.0.io_bytes - cursor.work.remaining().io_bytes <= 160);
            assert!(before.0.records - cursor.work.remaining().records <= 16);
            assert!(before.1 - cursor.budget.source_bytes_remaining() <= 160);
            assert!(before.2 - cursor.budget.steps_remaining() <= 256);
            turns += 1;
            if let Status::Complete(plan) = status {
                assert_eq!(
                    plan,
                    Selection {
                        plan: Some(Plan::Sections {
                            count: 24,
                            initial_encoded: false
                        }),
                        invalid_extended: false
                    }
                );
                break;
            }
            assert!(turns < 100_000);
        }
        // Every index replays the entire field rather than trusting max/count.
        assert!(
            crate::nfc::HeaderBudget::new().source_bytes_remaining()
                - cursor.budget.source_bytes_remaining()
                > 24 * source.len() as u64
        );
        let mut adversarial = String::from("attachment;filename=saved");
        for index in 0..128 {
            adversarial.push_str(&format!(";filename*{index}=value"));
        }
        let mut meter = work();
        let mut budget = crate::nfc::HeaderBudget::new();
        let mut credit = 0;
        let remaining = budget.source_bytes_remaining();
        budget
            .charge(&mut meter, Tick(1), remaining - 32_000, 0, &mut credit)
            .unwrap();
        let mut cursor = Budgeted::new(
            adversarial.as_bytes(),
            mime_fields::Kind::ContentDisposition,
            Attribute::Filename,
            &mut meter,
            &mut budget,
        );
        loop {
            match cursor.poll(Tick(1)) {
                Ok(Status::Yield) => {}
                Ok(Status::Complete(_)) => panic!("replay exhaustion admitted fallback"),
                Err(error) => {
                    assert_eq!(error, Error::InterpretationLimit);
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    break;
                }
            }
        }
    }
    #[test]
    fn independent_attributes_and_unicode_sections_keep_fixed_turns() {
        for (kind, attribute, source, expected) in [
            (
                mime_fields::Kind::ContentType,
                Attribute::Boundary,
                b"multipart/mixed;BoUnDaRy=CaseSensitive;boundary=ignored".as_slice(),
                b"CaseSensitive".as_slice(),
            ),
            (
                mime_fields::Kind::ContentType,
                Attribute::Name,
                b"text/plain;filename*=utf-8''other;NAME=kept",
                b"kept",
            ),
            (
                mime_fields::Kind::ContentDisposition,
                Attribute::Filename,
                b"attachment;filename*=utf-8''kept;name*01=other",
                b"utf-8''kept",
            ),
        ] {
            let mut cursor = Cursor::new(source, kind, attribute);
            let mut meter = work();
            let selection = loop {
                if let Status::Complete(selection) = cursor.poll(Tick(1), &mut meter).unwrap() {
                    break selection;
                }
            };
            let parameter = match selection.plan {
                Some(Plan::Ordinary(parameter) | Plan::Extended(parameter)) => parameter,
                _ => panic!("missing independent attribute"),
            };
            assert_eq!(
                source.get(parameter.value.start..parameter.value.end),
                Some(expected)
            );
            assert!(!selection.invalid_extended);
        }
        let source = format!(
            "attachment;filename*1=tail;filename*0=\"{}\"",
            "🐈".repeat(1024)
        );
        let mut meter = work();
        let mut budget = crate::nfc::HeaderBudget::new();
        let mut cursor = Budgeted::new(
            source.as_bytes(),
            mime_fields::Kind::ContentDisposition,
            Attribute::Filename,
            &mut meter,
            &mut budget,
        );
        let mut peak_visits = 0;
        loop {
            let before = (
                cursor.work.remaining(),
                cursor.budget.source_bytes_remaining(),
                cursor.budget.steps_remaining(),
            );
            let status = cursor.poll(Tick(1)).unwrap();
            let visits = before.1 - cursor.budget.source_bytes_remaining();
            peak_visits = peak_visits.max(visits);
            assert!(visits <= 160);
            assert!(before.0.io_bytes - cursor.work.remaining().io_bytes <= 160);
            assert!(before.0.records - cursor.work.remaining().records <= 16);
            assert!(before.2 - cursor.budget.steps_remaining() <= 256);
            if let Status::Complete(selection) = status {
                assert_eq!(
                    selection,
                    Selection {
                        plan: Some(Plan::Sections {
                            count: 2,
                            initial_encoded: false
                        }),
                        invalid_extended: false
                    }
                );
                break;
            }
        }
        assert_eq!(peak_visits, 160);
    }

    #[test]
    fn small_many_section_fields_can_exhaust_the_fresh_email_allowance() {
        for (count, succeeds) in [(400, true), (600, false)] {
            let mut source = String::from("attachment;filename=saved");
            for index in 0..count {
                source.push_str(&format!(";filename*{index}=x"));
            }
            let mut meter = work();
            let mut budget = crate::nfc::HeaderBudget::new();
            let initial_steps = budget.steps_remaining();
            let mut cursor = Budgeted::new(
                source.as_bytes(),
                mime_fields::Kind::ContentDisposition,
                Attribute::Filename,
                &mut meter,
                &mut budget,
            );
            loop {
                match cursor.poll(Tick(1)) {
                    Ok(Status::Yield) => {}
                    Ok(Status::Complete(selection)) => {
                        assert!(succeeds);
                        assert_eq!(
                            selection,
                            Selection {
                                plan: Some(Plan::Sections {
                                    count,
                                    initial_encoded: false
                                }),
                                invalid_extended: false
                            }
                        );
                        assert_eq!(source.len(), 5915);
                        assert_eq!(initial_steps - cursor.budget.steps_remaining(), 15_759_691);
                        break;
                    }
                    Err(error) => {
                        assert!(!succeeds);
                        assert_eq!(source.len(), 8915);
                        assert_eq!(error, Error::InterpretationLimit);
                        assert_eq!(cursor.poll(Tick(1)), Err(error));
                        assert_eq!(
                            cursor
                                .budget
                                .charge(cursor.work, Tick(1), 0, 0, &mut cursor.credit),
                            Err(crate::nfc::Error::InterpretationLimit)
                        );
                        break;
                    }
                }
            }
        }
    }

    #[test]
    fn every_aggregate_and_job_cut_refuses_instead_of_falling_back() {
        let source = b"attachment;filename=saved;filename*1*=two;filename*0*=utf-8''one";
        let mut full_work = work();
        let mut full_budget = crate::nfc::HeaderBudget::new();
        let before = (
            full_work.remaining(),
            full_budget.source_bytes_remaining(),
            full_budget.steps_remaining(),
        );
        let mut full = Budgeted::new(
            source,
            mime_fields::Kind::ContentDisposition,
            Attribute::Filename,
            &mut full_work,
            &mut full_budget,
        );
        assert!(std::mem::size_of_val(&full) <= 1056);
        loop {
            let turn = (
                full.work.remaining(),
                full.budget.source_bytes_remaining(),
                full.budget.steps_remaining(),
                full.credit,
            );
            full.check_deadline(Tick(1)).unwrap();
            assert_eq!(
                (
                    full.work.remaining(),
                    full.budget.source_bytes_remaining(),
                    full.budget.steps_remaining(),
                    full.credit
                ),
                turn
            );
            let status = full.poll(Tick(1)).unwrap();
            assert!(turn.0.io_bytes - full.work.remaining().io_bytes <= 160);
            assert!(turn.0.records - full.work.remaining().records <= 16);
            assert!(turn.1 - full.budget.source_bytes_remaining() <= 160);
            assert!(turn.2 - full.budget.steps_remaining() <= 256);
            if let Status::Complete(p) = status {
                assert!(matches!(
                    p.plan,
                    Some(Plan::Sections {
                        count: 2,
                        initial_encoded: true
                    })
                ));
                assert_eq!(full.poll(Tick(100)), Ok(Status::Complete(p)));
                break;
            }
        }
        let visits = before.1 - full.budget.source_bytes_remaining();
        let steps = before.2 - full.budget.steps_remaining();
        let records = before.0.records - full.work.remaining().records;
        full.check_deadline(Tick(100)).unwrap_err();
        assert_eq!(full.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
        for flavor in 0..4 {
            let amount = match flavor {
                0 | 2 => visits,
                1 => steps,
                _ => records,
            };
            for limit in 0..amount {
                let mut admission = if flavor >= 2 {
                    Meter::new(
                        Deadline::after(Tick(0), 100).unwrap(),
                        Charge {
                            io_bytes: if flavor == 2 { limit } else { visits },
                            records: if flavor == 3 { limit } else { records },
                            ..Charge::default()
                        },
                    )
                } else {
                    work()
                };
                let mut budget = crate::nfc::HeaderBudget::new();
                let mut credit = 0;
                if flavor < 2 {
                    let (bytes, steps) = if flavor == 0 {
                        (budget.source_bytes_remaining() - limit, 0)
                    } else {
                        (0, budget.steps_remaining() - limit)
                    };
                    budget
                        .charge(&mut admission, Tick(1), bytes, steps, &mut credit)
                        .unwrap();
                }
                let mut cursor = Budgeted::new(
                    source,
                    mime_fields::Kind::ContentDisposition,
                    Attribute::Filename,
                    &mut admission,
                    &mut budget,
                );
                loop {
                    match cursor.poll(Tick(1)) {
                        Ok(Status::Complete(_)) => panic!("cut admitted candidate or fallback"),
                        Ok(Status::Yield) => {}
                        Err(e) => {
                            assert_eq!(
                                e,
                                match flavor {
                                    0 | 1 => Error::InterpretationLimit,
                                    2 => Error::Work(Stop::IoBytes),
                                    _ => Error::Work(Stop::Records),
                                }
                            );
                            assert_eq!(cursor.check_deadline(Tick(1)), Err(e));
                            assert_eq!(cursor.poll(Tick(1)), Err(e));
                            break;
                        }
                    }
                }
            }
        }
    }
}
