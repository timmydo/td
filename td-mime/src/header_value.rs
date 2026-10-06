//! Provisional header property JSON for an unpublished response-spool tail.
use crate::{
    header_property::{Form, Occurrence, Property},
    header_raw, header_select,
    headers::{End, Field},
    json_string::{self, Frame},
    nfc::{self, HeaderBudget},
    time::Tick,
    work::{Charge, Meter, Stop},
};
#[path = "header_value/dispatch.rs"]
mod dispatch;
pub use dispatch::Cursor;
#[path = "header_value/list.rs"]
mod list;
use list::{IdsMode, UrlsMode};
#[path = "header_value/addresses.rs"]
mod addresses;
use addresses::{AddressMode, GroupedMode};
#[path = "header_value/date.rs"]
mod date;
use date::DateMode;
#[path = "header_value/projection.rs"]
mod projection;
use projection::{Projection, RawMode, TextMode};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    UnsupportedForm,
    UnsupportedGrammar,
    Selection(header_select::Error),
    Raw(header_raw::Error),
    Addresses(crate::header_addresses::Error),
    AddressText(crate::header_address_text::Error),
    Name(crate::header_name::Error),
    Text(nfc::Error),
    MessageIds(crate::header_message_ids::Error),
    URLs(crate::header_urls::Error),
    Date(crate::header_date::Error),
    DateProjection(crate::header_date::project::Error),
    Json(json_string::Error),
    Work(Stop),
    InterpretationLimit,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedGrammar => f.write_str("unsupported header value grammar"),
            Self::UnsupportedForm => f.write_str("unsupported header value form"),
            Self::Selection(error) => write!(f, "header value selection: {error}"),
            Self::Addresses(error) => write!(f, "header value addresses: {error}"),
            Self::AddressText(error) => write!(f, "header value address text: {error}"),
            Self::Name(error) => write!(f, "header value display name: {error}"),
            Self::Raw(error) => write!(f, "header value Raw: {error}"),
            Self::URLs(error) => write!(f, "header value URLs: {error}"),
            Self::MessageIds(error) => write!(f, "header value MessageIds: {error}"),
            Self::Date(error) => write!(f, "header value Date: {error}"),
            Self::DateProjection(error) => write!(f, "header value Date projection: {error}"),
            Self::Text(error) => write!(f, "header value Text: {error}"),
            Self::Json(error) => write!(f, "header value JSON: {error}"),
            Self::Work(error) => write!(f, "header value work: {error}"),
            Self::InterpretationLimit => f.write_str("header value interpretation limit"),
            Self::InvalidState => f.write_str("invalid header value state"),
        }
    }
}
impl std::error::Error for Error {}
impl From<crate::header_message_ids::Error> for Error {
    fn from(error: crate::header_message_ids::Error) -> Self {
        Self::MessageIds(error)
    }
}
impl From<crate::header_urls::Error> for Error {
    fn from(error: crate::header_urls::Error) -> Self {
        Self::URLs(error)
    }
}
impl From<nfc::Error> for Error {
    fn from(error: nfc::Error) -> Self {
        match error {
            nfc::Error::Work(stop) => Self::Work(stop),
            nfc::Error::InterpretationLimit => Self::InterpretationLimit,
            nfc::Error::InvalidState | nfc::Error::InvalidTable => Self::InvalidState,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    NeedOutput,
    Complete(End),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub written: usize,
    pub status: Status,
}
#[derive(Clone, Copy)]
enum Phase {
    Start,
    Select,
    Value,
    Release,
    Drain,
    Complete,
}
/// Immutable selection input; the owner authorizes and retains these bytes.
pub struct Input<'a> {
    pub bytes: &'a [u8],
    pub base: u64,
    pub header_limit: u64,
    pub property: Property<'a>,
    pub source_end: header_select::SourceEnd,
}
enum Owner<'a, 'w, P: Projection<'a, 'w>> {
    Budgets(&'w mut Meter, &'w mut HeaderBudget, P::Workspace),
    Value(P::Source),
    Retired,
}
/// Provisional Raw property; retain chunks in an unpublished response tail.
/// Borrows the same job/email budgets across selection and values.
pub struct Raw<'a, 'w>(Core<'a, 'w, RawMode>);
impl<'a, 'w> Raw<'a, 'w> {
    pub fn new(
        input: &'a [u8],
        base: u64,
        header_limit: u64,
        property: Property<'a>,
        source_end: header_select::SourceEnd,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Result<Self, Error> {
        Core::new(
            Input {
                bytes: input,
                base,
                header_limit,
                property,
                source_end,
            },
            work,
            budget,
            (),
        )
        .map(Self)
    }
    pub const fn is_encoding_problem(&self) -> bool {
        self.0.is_encoding_problem()
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.0.check_deadline(now)
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.0.poll(now, output)
    }
}
/// Provisional normalized Text property using the existing caller-owned NFC scratch.
pub struct Text<'a, 'w>(Core<'a, 'w, TextMode>);
impl<'a, 'w> Text<'a, 'w> {
    pub fn new(
        input: Input<'a>,
        scratch: &'w mut nfc::Scratch,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Result<Self, Error> {
        Core::new(
            input,
            work,
            budget,
            (scratch, crate::header_text::Grammar::Text),
        )
        .map(Self)
    }
    pub const fn is_encoding_problem(&self) -> bool {
        self.0.is_encoding_problem()
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.0.check_deadline(now)
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.0.poll(now, output)
    }
}
/// Provisional Date property using bounded caller-independent inline staging.
pub struct Date<'a, 'w>(Core<'a, 'w, DateMode>);
impl<'a, 'w> Date<'a, 'w> {
    pub fn new(
        input: Input<'a>,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Result<Self, Error> {
        Core::new(input, work, budget, ()).map(Self)
    }
    /// Final only after Complete; an unqualified :60 produced a null value.
    pub const fn has_unverified_leap(&self) -> bool {
        self.0.unverified_leap
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.0.check_deadline(now)
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.0.poll(now, output)
    }
}
/// Provisional arrays of MessageIds, retaining whole-field validation.
pub struct MessageIds<'a, 'w>(Core<'a, 'w, IdsMode>);
impl<'a, 'w> MessageIds<'a, 'w> {
    pub fn new(
        input: Input<'a>,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Result<Self, Error> {
        Core::new(input, work, budget, crate::header_message_ids::Mode::Strict).map(Self)
    }
    /// Final only after property Complete.
    pub const fn is_encoding_problem(&self) -> bool {
        self.0.is_encoding_problem()
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.0.check_deadline(now)
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.0.poll(now, output)
    }
}
/// Provisional arrays of validated URL strings; no fetching is authorized.
pub struct URLs<'a, 'w>(Core<'a, 'w, UrlsMode>);
impl<'a, 'w> URLs<'a, 'w> {
    pub fn new(
        input: Input<'a>,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Result<Self, Error> {
        Core::new(input, work, budget, crate::header_urls::Mode::URLs).map(Self)
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.0.check_deadline(now)
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.0.poll(now, output)
    }
}
/// Provisional flattened address objects; retain in an unpublished response tail.
pub struct Addresses<'a, 'w>(Core<'a, 'w, AddressMode>);
impl<'a, 'w> Addresses<'a, 'w> {
    pub fn new(
        input: Input<'a>,
        scratch: &'w mut nfc::Scratch,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Result<Self, Error> {
        Core::new(input, work, budget, scratch).map(Self)
    }
    /// Final only after property Complete.
    pub const fn is_encoding_problem(&self) -> bool {
        self.0.is_encoding_problem()
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.0.check_deadline(now)
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.0.poll(now, output)
    }
}
/// Provisional grouped address objects; retain in an unpublished response tail.
pub struct GroupedAddresses<'a, 'w>(Core<'a, 'w, GroupedMode>);
impl<'a, 'w> GroupedAddresses<'a, 'w> {
    pub fn new(
        input: Input<'a>,
        scratch: &'w mut nfc::Scratch,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Result<Self, Error> {
        Core::new(input, work, budget, scratch).map(Self)
    }
    /// Final only after property Complete.
    pub const fn is_encoding_problem(&self) -> bool {
        self.0.is_encoding_problem()
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.0.check_deadline(now)
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.0.poll(now, output)
    }
}
struct Core<'a, 'w, P: Projection<'a, 'w>> {
    input: &'a [u8],
    name: &'a str,
    base: u64,
    selector: header_select::Cursor<'a>,
    owner: Owner<'a, 'w, P>,
    frame: Frame,
    phase: Phase,
    next: Phase,
    literal: [u8; 4],
    used: usize,
    position: usize,
    all: bool,
    seen: bool,
    problem: bool,
    unverified_leap: bool,
    end: Option<End>,
    failure: Option<Error>,
}
impl<'a, 'w, P: Projection<'a, 'w>> Core<'a, 'w, P> {
    fn new(
        input: Input<'a>,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
        workspace: P::Workspace,
    ) -> Result<Self, Error> {
        if input.property.form() != P::FORM {
            return Err(Error::UnsupportedForm);
        }
        Ok(Self {
            input: input.bytes,
            name: input.property.name(),
            base: input.base,
            selector: header_select::Cursor::new(
                input.bytes,
                input.base,
                input.header_limit,
                input.property,
                input.source_end,
            ),
            owner: Owner::Budgets(work, budget, workspace),
            frame: Frame::new(),
            phase: Phase::Start,
            next: Phase::Start,
            literal: [0; 4],
            used: 0,
            position: 0,
            all: input.property.occurrence() == Occurrence::All,
            seen: false,
            problem: false,
            unverified_leap: false,
            end: None,
            failure: None,
        })
    }
    /// Final only after Complete; output remains provisional at method level.
    pub const fn is_encoding_problem(&self) -> bool {
        self.problem
    }
    fn charge_output(&mut self, now: Tick, output_bytes: u64) -> Result<(), Error> {
        match &mut self.owner {
            Owner::Budgets(work, budget, _) => {
                budget.charge_local(work, now, 0, 0, &mut 0)?;
                work.charge(
                    now,
                    Charge {
                        output_bytes,
                        ..Charge::default()
                    },
                )
                .map_err(Error::Work)
            }
            Owner::Value(source) => P::charge_output(source, now, output_bytes),
            Owner::Retired => Err(Error::InvalidState),
        }
    }
    /// Refusal invalidates the provisional value even after cached completion.
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.charge_output(now, 0);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return self.completed(0);
        }
        let result = self.step(now, output);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn completed(&self, written: usize) -> Result<Progress, Error> {
        Ok(Progress {
            written,
            status: Status::Complete(self.end.ok_or(Error::InvalidState)?),
        })
    }
    fn stage(&mut self, now: Tick, bytes: &[u8], next: Phase) -> Result<(), Error> {
        if bytes.is_empty() || bytes.len() > self.literal.len() {
            return Err(Error::InvalidState);
        }
        self.charge_output(now, bytes.len() as u64)?;
        self.literal
            .get_mut(..bytes.len())
            .ok_or(Error::InvalidState)?
            .copy_from_slice(bytes);
        self.used = bytes.len();
        self.position = 0;
        self.next = next;
        self.phase = Phase::Drain;
        Ok(())
    }
    fn begin_value(&mut self, field: Field) -> Result<(), Error> {
        let bytes =
            td_header::resident::slice(self.input, self.base, field.value_start..field.value_end)
                .ok_or(Error::InvalidState)?;
        let Owner::Budgets(work, budget, workspace) =
            std::mem::replace(&mut self.owner, Owner::Retired)
        else {
            return Err(Error::InvalidState);
        };
        self.owner = Owner::Value(P::start(bytes, work, budget, workspace));
        self.frame = Frame::new();
        Ok(())
    }
    fn step(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.check_deadline(now)?;
        if output.is_empty() {
            return Ok(Progress {
                written: 0,
                status: Status::NeedOutput,
            });
        }
        match self.phase {
            Phase::Start => {
                let Owner::Budgets(work, budget, workspace) = &mut self.owner else {
                    return Err(Error::InvalidState);
                };
                P::validate(self.name, now, work, budget, workspace)?;
                if self.all {
                    self.stage(now, b"[", Phase::Select)?;
                } else {
                    self.phase = Phase::Select;
                }
            }
            Phase::Select => {
                let Owner::Budgets(work, budget, _) = &mut self.owner else {
                    return Err(Error::InvalidState);
                };
                match self
                    .selector
                    .poll_with_budget(now, work, budget)
                    .map_err(Error::Selection)?
                {
                    header_select::Status::Yield => {}
                    header_select::Status::Match(field) => {
                        if !self.all && self.seen {
                            return Err(Error::InvalidState);
                        }
                        self.begin_value(field)?;
                        if self.all && self.seen {
                            self.stage(now, b",", Phase::Value)?;
                        } else {
                            self.phase = Phase::Value;
                        }
                        self.seen = true;
                    }
                    header_select::Status::Complete(end) => {
                        self.end = Some(end);
                        if self.all {
                            self.stage(now, b"]", Phase::Complete)?;
                        } else if !self.seen {
                            self.stage(now, b"null", Phase::Complete)?;
                        } else {
                            self.phase = Phase::Complete;
                            return self.completed(0);
                        }
                    }
                }
            }
            Phase::Value => {
                let Owner::Value(source) = &mut self.owner else {
                    return Err(Error::InvalidState);
                };
                let progress = P::poll(source, &mut self.frame, now, output)?;
                if progress.status == json_string::Status::Complete {
                    self.problem |= P::is_encoding_problem(source);
                    self.unverified_leap |= P::has_unverified_leap(source);
                    self.phase = Phase::Release;
                }
                return Ok(Progress {
                    written: progress.written,
                    status: Status::Yield,
                });
            }
            Phase::Release => {
                let Owner::Value(source) = std::mem::replace(&mut self.owner, Owner::Retired)
                else {
                    return Err(Error::InvalidState);
                };
                let (work, budget, workspace) = P::finish(source)?;
                self.owner = Owner::Budgets(work, budget, workspace);
                self.phase = Phase::Select;
            }
            Phase::Drain => {
                let count = self
                    .used
                    .checked_sub(self.position)
                    .ok_or(Error::InvalidState)?
                    .min(output.len());
                let end = self
                    .position
                    .checked_add(count)
                    .ok_or(Error::InvalidState)?;
                output
                    .get_mut(..count)
                    .ok_or(Error::InvalidState)?
                    .copy_from_slice(
                        self.literal
                            .get(self.position..end)
                            .ok_or(Error::InvalidState)?,
                    );
                self.position = end;
                if end == self.used {
                    self.phase = self.next;
                }
                if matches!(self.phase, Phase::Complete) {
                    return self.completed(count);
                }
                return Ok(Progress {
                    written: count,
                    status: Status::Yield,
                });
            }
            Phase::Complete => return self.completed(0),
        }
        Ok(Progress {
            written: 0,
            status: Status::Yield,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{header_property, time::Deadline};
    fn work() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 2_000_000,
                output_bytes: 10_000_000,
                ..Charge::default()
            },
        )
    }
    fn property(key: &str) -> Property<'_> {
        let mut cursor = header_property::Cursor::new(key, header_property::Context::Email);
        let mut work = work();
        for _ in 0..1000 {
            if let header_property::Status::Complete(value) =
                cursor.poll(Tick(1), &mut work).unwrap()
            {
                return value.unwrap();
            }
        }
        panic!("property did not complete");
    }
    pub(super) fn drain(cursor: &mut Raw<'_, '_>, width: usize) -> (Vec<u8>, End) {
        assert!(std::mem::size_of_val(cursor) <= 640);
        let mut output = [0xa5; 16];
        let mut bytes = Vec::new();
        for _ in 0..100_000 {
            output.fill(0xa5);
            let progress = cursor.poll(Tick(1), &mut output[..width]).unwrap();
            assert!(progress.written <= 6);
            assert!(output[progress.written..].iter().all(|byte| *byte == 0xa5));
            bytes.extend_from_slice(&output[..progress.written]);
            if let Status::Complete(end) = progress.status {
                assert_eq!(
                    cursor.poll(Tick(100), &mut []),
                    Ok(Progress {
                        written: 0,
                        status: Status::Complete(end)
                    })
                );
                cursor.check_deadline(Tick(1)).unwrap();
                return (bytes, end);
            }
        }
        panic!("value did not finish");
    }
    #[test]
    fn last_all_absent_empty_and_repaired_fields_form_complete_json() {
        let input = b"X: first\r\nx:e\xcc\x81\r\n\tfold\0\xff\r\nX:\r\n\r\nbody";
        for width in 1..=8 {
            for base in [0, 4096] {
                for (key, expected, problem) in [
                    ("header:X", "\"\"", false),
                    (
                        "header:X:all",
                        "[\" first\",\"e\u{301}\\r\\n\\tfold�\",\"\"]",
                        true,
                    ),
                    ("header:Missing", "null", false),
                    ("header:Missing:all", "[]", false),
                ] {
                    let mut work = work();
                    let mut budget = HeaderBudget::new();
                    let before = work.remaining();
                    let mut cursor = Raw::new(
                        input,
                        base,
                        1000,
                        property(key),
                        header_select::SourceEnd::Prefix,
                        &mut work,
                        &mut budget,
                    )
                    .unwrap();
                    let (bytes, end) = drain(&mut cursor, width);
                    assert_eq!(bytes, expected.as_bytes());
                    assert_eq!(end.body_start, base + input.len() as u64 - 4);
                    assert_eq!(cursor.is_encoding_problem(), problem);
                    assert_eq!(
                        before.output_bytes - work.remaining().output_bytes,
                        bytes.len() as u64
                    );
                    assert!(budget.source_bytes_remaining() < 16 * 1024 * 1024);
                }
            }
        }
        for (input, key, expected) in [
            (b"".as_slice(), "header:X", "null"),
            (b"X:a", "header:X:all", "[\"a\"]"),
        ] {
            let mut work = work();
            let mut budget = HeaderBudget::new();
            let mut cursor = Raw::new(
                input,
                0,
                100,
                property(key),
                header_select::SourceEnd::Eof,
                &mut work,
                &mut budget,
            )
            .unwrap();
            assert_eq!(drain(&mut cursor, 1).0, expected.as_bytes());
        }
    }
    #[test]
    fn selection_and_values_share_exact_live_credits_across_handoffs() {
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let before = work.remaining();
        let mut cursor = Raw::new(
            b"X:a\nX:b\n\n",
            0,
            100,
            property("header:X:all"),
            header_select::SourceEnd::Eof,
            &mut work,
            &mut budget,
        )
        .unwrap();
        assert_eq!(drain(&mut cursor, 1).0, b"[\"a\",\"b\"]");
        // Selection: 15 visits, 16 steps; each Raw value: one visit, five steps.
        assert_eq!(budget.source_bytes_remaining(), 16 * 1024 * 1024 - 17);
        assert_eq!(budget.steps_remaining(), 16_000_000 - 26);
        assert_eq!(before.io_bytes - work.remaining().io_bytes, 17);
        assert_eq!(before.records - work.remaining().records, 3);
        assert_eq!(before.output_bytes - work.remaining().output_bytes, 9);
    }
    fn refuse(cursor: &mut Raw<'_, '_>, width: usize) -> (Error, Vec<u8>) {
        let mut output = [0xa5; 8];
        let mut prefix = Vec::new();
        for _ in 0..10_000 {
            match cursor.poll(Tick(1), &mut output[..width]) {
                Ok(progress) => {
                    assert!(!matches!(progress.status, Status::Complete(_)));
                    prefix.extend_from_slice(&output[..progress.written]);
                }
                Err(error) => {
                    output.fill(0xa5);
                    assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                    assert_eq!(output, [0xa5; 8]);
                    return (error, prefix);
                }
            }
        }
        panic!("value did not refuse");
    }
    #[test]
    fn late_scan_failure_discards_provisional_values_and_budget_refusal_is_sticky() {
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let mut cursor = Raw::new(
            b"X:a\nOther: too long\n\n",
            0,
            5,
            property("header:X:all"),
            header_select::SourceEnd::Eof,
            &mut work,
            &mut budget,
        )
        .unwrap();
        let (error, prefix) = refuse(&mut cursor, 1);
        assert_eq!(
            error,
            Error::Selection(header_select::Error::Headers(
                crate::headers::Error::HeaderLimit
            ))
        );
        assert_eq!(prefix, b"[\"a\"");
        let mut work = self::work();
        let mut budget = HeaderBudget::new();
        let mut cursor = Raw::new(
            b"X:a\nY:b",
            0,
            100,
            property("header:X:all"),
            header_select::SourceEnd::Prefix,
            &mut work,
            &mut budget,
        )
        .unwrap();
        let (error, prefix) = refuse(&mut cursor, 1);
        assert_eq!(error, Error::Selection(header_select::Error::Truncated));
        assert_eq!(prefix, b"[\"a\"");
        let mut work = self::work();
        let mut budget = HeaderBudget::new();
        budget
            .charge_local(&mut work, Tick(1), 0, budget.steps_remaining(), &mut 0)
            .unwrap();
        let mut cursor = Raw::new(
            b"X:a",
            0,
            100,
            property("header:X"),
            header_select::SourceEnd::Eof,
            &mut work,
            &mut budget,
        )
        .unwrap();
        assert_eq!(
            refuse(&mut cursor, 1).0,
            Error::Selection(header_select::Error::InterpretationLimit)
        );
        assert_eq!(work.remaining().output_bytes, 10_000_000);
        let mut work = self::work();
        let mut budget = HeaderBudget::new();
        budget
            .charge_local(&mut work, Tick(1), 0, budget.steps_remaining() - 7, &mut 0)
            .unwrap();
        let mut cursor = Raw::new(
            b"X:a\n\n",
            0,
            100,
            property("header:X:all"),
            header_select::SourceEnd::Eof,
            &mut work,
            &mut budget,
        )
        .unwrap();
        let (error, prefix) = refuse(&mut cursor, 1);
        assert_eq!(
            error,
            Error::Json(json_string::Error::Raw(
                header_raw::Error::InterpretationLimit
            ))
        );
        assert_eq!(prefix, b"[\"");
        assert_eq!(work.stopped(), None);
    }
    #[test]
    fn output_refusal_backpressure_and_final_deadline_never_publish_partial_success() {
        for capacity in 0..9 {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: 10000,
                    records: 1000,
                    output_bytes: capacity,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            let mut cursor = Raw::new(
                b"X:a\nX:b\n\n",
                0,
                100,
                property("header:X:all"),
                header_select::SourceEnd::Eof,
                &mut work,
                &mut budget,
            )
            .unwrap();
            let (error, prefix) = refuse(&mut cursor, 1);
            let expected = match capacity {
                0 | 8 => Error::Work(Stop::OutputBytes),
                4 => Error::Raw(header_raw::Error::Work(Stop::OutputBytes)),
                _ => Error::Json(json_string::Error::Raw(header_raw::Error::Work(
                    Stop::OutputBytes,
                ))),
            };
            assert_eq!(error, expected);
            assert!(prefix.len() <= capacity as usize);
            assert_eq!(work.stopped(), Some(Stop::OutputBytes));
        }
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let mut cursor = Raw::new(
            b"",
            0,
            100,
            property("header:X"),
            header_select::SourceEnd::Eof,
            &mut work,
            &mut budget,
        )
        .unwrap();
        for _ in 0..10 {
            assert_eq!(
                cursor.poll(Tick(1), &mut []),
                Ok(Progress {
                    written: 0,
                    status: Status::NeedOutput
                })
            );
        }
        assert!(matches!(cursor.0.phase, Phase::Start));
        assert_eq!(drain(&mut cursor, 1).0, b"null");
        assert_eq!(
            cursor.check_deadline(Tick(100)),
            Err(Error::Work(Stop::Deadline))
        );
        assert_eq!(
            cursor.poll(Tick(1), &mut [0]),
            Err(Error::Work(Stop::Deadline))
        );
        let mut work = self::work();
        let mut budget = HeaderBudget::new();
        assert!(matches!(
            Raw::new(
                b"X:a",
                0,
                100,
                property("subject"),
                header_select::SourceEnd::Eof,
                &mut work,
                &mut budget
            ),
            Err(Error::UnsupportedForm)
        ));
        assert_eq!(work.remaining().output_bytes, 10_000_000);
    }
}

#[cfg(test)]
#[path = "header_value/text_tests.rs"]
mod text_tests;

#[cfg(test)]
#[path = "header_value/date_tests.rs"]
mod date_tests;

#[cfg(test)]
#[path = "header_value/message_ids_tests.rs"]
mod message_ids_tests;

#[cfg(test)]
#[path = "header_value/urls_tests.rs"]
mod urls_tests;

#[cfg(test)]
#[path = "header_value/addresses_tests.rs"]
mod addresses_tests;

#[cfg(test)]
#[path = "header_value/grouped_tests.rs"]
mod grouped_tests;

#[cfg(test)]
#[path = "header_value/dispatch_tests.rs"]
mod dispatch_tests;
