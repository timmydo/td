//! Provisional literal parameter scalars; placement and NFC stay with the owner.
use super::{Attribute, Error, OctetStatus, Octets, Plan, Selection};
use crate::{
    admission::work::{Charge, Meter},
    decode_work::{self, Work},
    mime_charset::{self, Charset, Decoder, Label},
    mime_fields::Kind,
    mime_value::Role,
    ports::Tick,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Decoded {
    pub selection: Selection,
    /// Charset label recovery or malformed charset data, not family diagnostics.
    pub is_encoding_problem: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Scalar(char),
    Complete(Decoded),
}
#[derive(Clone, Copy)]
enum Phase {
    Start,
    Read,
    Octet,
    Decode,
    Finish,
    Complete,
}
/// No byte vector or label buffer; publish scalars only after charged completion.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_parameter::scalars::Cursor<'_>>();
/// ```
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_parameter::scalars::Cursor<'_>>();
/// ```
pub struct Cursor<'a> {
    octets: Octets<'a>,
    attribute: Attribute,
    phase: Phase,
    label: Label,
    held: Option<(Role, u8)>,
    decoder: Option<Decoder>,
    selection: Option<Selection>,
    eof: bool,
    problem: bool,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    #[must_use]
    pub const fn new(source: &'a [u8], kind: Kind, attribute: Attribute) -> Self {
        Self {
            octets: Octets::new(source, kind, attribute),
            attribute,
            phase: Phase::Start,
            label: Label::new(),
            held: None,
            decoder: None,
            selection: None,
            eof: false,
            problem: false,
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
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = admit().map_err(Error::from);
        if let Err(error) = result {
            self.failure = Some(error);
            self.selection = None;
            self.held = None;
        }
        result
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        self.poll_with_work(now, work)
    }
    pub(super) fn poll_with_work(
        &mut self,
        now: Tick,
        work: &mut impl Work,
    ) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete(self.decoded()?));
        }
        let result = self.step(now, work);
        if let Err(error) = result {
            self.failure = Some(error);
            self.selection = None;
            self.held = None;
        }
        result
    }
    pub(super) fn ordinary_ready(&self) -> Option<Selection> {
        if self.failure.is_some()
            || !matches!(self.phase, Phase::Read)
            || self.held.is_some()
            || self.decoder.is_some()
        {
            return None;
        }
        self.octets
            .validated_selection()
            .filter(|selection| matches!(selection.plan, Some(Plan::Ordinary(_))))
    }
    fn decoded(&self) -> Result<Decoded, Error> {
        Ok(Decoded {
            selection: self.selection.ok_or(Error::InvalidState)?,
            is_encoding_problem: self.problem
                || self
                    .decoder
                    .as_ref()
                    .is_some_and(Decoder::is_encoding_problem),
        })
    }
    fn select_decoder(&mut self) -> Result<(), Error> {
        if self.decoder.is_some() {
            return Ok(());
        }
        let selection = self
            .selection
            .or(self.octets.validated_selection())
            .ok_or(Error::InvalidState)?;
        // Only an encoded initial value carries prefix roles; later values are Data.
        let labelled = matches!(
            selection.plan,
            Some(
                Plan::Extended(_)
                    | Plan::Sections {
                        initial_encoded: true,
                        ..
                    }
            )
        );
        let charset = if labelled {
            let chosen = self.label.finish();
            self.problem |= chosen.is_none();
            chosen.unwrap_or(Charset::Utf8)
        } else {
            Charset::Utf8
        };
        self.decoder = Some(Decoder::new(charset));
        Ok(())
    }
    fn step(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
        match self.phase {
            Phase::Start => {
                work.charge(
                    now,
                    Charge {
                        records: 1,
                        ..Charge::default()
                    },
                )?;
                if !matches!(self.attribute, Attribute::Name | Attribute::Filename) {
                    return Err(Error::InvalidState);
                }
                self.phase = Phase::Read;
            }
            Phase::Read => match self.octets.poll_with_work(now, work)? {
                OctetStatus::Yield => {}
                OctetStatus::Octet { role, value } => {
                    self.held = Some((role, value));
                    self.phase = Phase::Octet;
                }
                OctetStatus::Complete(selection) => {
                    self.selection = Some(selection);
                    self.eof = true;
                    self.select_decoder()?;
                    self.phase = Phase::Decode;
                }
            },
            Phase::Octet => {
                let (role, value) = self.held.ok_or(Error::InvalidState)?;
                work.charge(
                    now,
                    Charge {
                        records: if role == Role::Charset {
                            Label::FEED_RECORDS
                        } else {
                            1
                        },
                        ..Charge::default()
                    },
                )?;
                if role != Role::Data && self.decoder.is_some() {
                    return Err(Error::InvalidState);
                }
                match role {
                    Role::Charset => {
                        self.label.feed(value);
                        self.held = None;
                        self.phase = Phase::Read;
                    }
                    Role::Language => {
                        self.held = None;
                        self.phase = Phase::Read;
                    }
                    Role::Data => {
                        self.select_decoder()?;
                        self.phase = Phase::Decode;
                    }
                }
            }
            Phase::Decode => {
                let input = [self.held.map(|(_, value)| value).unwrap_or(0)];
                let length = usize::from(self.held.is_some());
                let progress = self
                    .decoder
                    .as_mut()
                    .ok_or(Error::InvalidState)?
                    .poll_with_work(
                        input.get(..length).ok_or(Error::InvalidState)?,
                        self.eof,
                        now,
                        work,
                    )?;
                if progress.consumed > length {
                    return Err(Error::InvalidState);
                }
                if progress.consumed == 1 {
                    self.held = None;
                }
                match progress.status {
                    mime_charset::Status::NeedInput => {
                        if self.eof || self.held.is_some() {
                            return Err(Error::InvalidState);
                        }
                        self.phase = Phase::Read;
                    }
                    mime_charset::Status::Scalar(value) => {
                        if self.held.is_none() && !self.eof {
                            self.phase = Phase::Read;
                        }
                        return Ok(Status::Scalar(value));
                    }
                    mime_charset::Status::Complete => {
                        if !self.eof || self.held.is_some() {
                            return Err(Error::InvalidState);
                        }
                        self.phase = Phase::Finish;
                    }
                }
            }
            Phase::Finish => {
                work.charge(
                    now,
                    Charge {
                        records: 1,
                        ..Charge::default()
                    },
                )?;
                let decoded = self.decoded()?;
                self.phase = Phase::Complete;
                return Ok(Status::Complete(decoded));
            }
            Phase::Complete => return Err(Error::InvalidState),
        }
        Ok(Status::Yield)
    }
}
/// Same original job/header allowance and credit across replay and conversion.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_parameter::scalars::Budgeted<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_parameter::scalars::Budgeted<'_, '_>>();
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
        kind: Kind,
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
    use crate::{admission::work::Stop, ports::Deadline};
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
    fn decode(source: &[u8]) -> Result<(String, Decoded), Error> {
        decode_kind(source, Kind::ContentDisposition, Attribute::Filename)
    }
    fn decode_kind(
        source: &[u8],
        kind: Kind,
        attribute: Attribute,
    ) -> Result<(String, Decoded), Error> {
        let mut cursor = Cursor::new(source, kind, attribute);
        assert!(std::mem::size_of_val(&cursor) <= 1280);
        let mut meter = work();
        let mut output = String::new();
        loop {
            let before = meter.remaining();
            let status = cursor.poll(Tick(1), &mut meter)?;
            assert!(before.io_bytes - meter.remaining().io_bytes <= 160);
            assert!(before.records - meter.remaining().records <= 33);
            match status {
                Status::Yield => {}
                Status::Scalar(value) => output.push(value),
                Status::Complete(decoded) => {
                    let before = meter.remaining();
                    assert_eq!(
                        cursor.poll(Tick(100), &mut meter),
                        Ok(Status::Complete(decoded))
                    );
                    assert_eq!(before, meter.remaining());
                    assert_eq!(
                        cursor.check_deadline(Tick(100), &mut meter),
                        Err(Error::Work(Stop::Deadline))
                    );
                    assert_eq!(
                        cursor.poll(Tick(1), &mut work()),
                        Err(Error::Work(Stop::Deadline))
                    );
                    return Ok((output, decoded));
                }
            }
        }
    }
    #[test]
    fn known_labels_and_one_decoder_across_mixed_sections() {
        for (source, expected, problem) in [
            (
                b"attachment;filename*2*=%AC;filename*0*=UTF-8'en'%E2;filename*1*=%82".as_slice(),
                "€",
                false,
            ),
            (
                b"attachment;filename*0*=utf8''%E2;filename*1=\"\\\\\";filename*2*=%82%AC",
                "�\\��",
                true,
            ),
            (b"attachment;filename*=latin1''%E9%FF", "éÿ", false),
            (
                b"attachment;filename*0*=latin1''%E9;filename*1=\"\xc3\xa9%41\"",
                "éÃ©%41",
                false,
            ),
            (
                b"attachment;filename*0*=UTF-8''%E2;filename*1=\"\";filename*2*=%82%AC",
                "€",
                false,
            ),
            (b"attachment;filename*=cp1252''%80%81", "€�", true),
            (b"attachment;filename*=us-ascii''%FFa", "�a", true),
            (b"attachment;filename*=UTF-8''%F0%9F%90%88", "🐈", false),
        ] {
            let (out, decoded) = decode(source).unwrap();
            assert_eq!(out, expected);
            assert_eq!(decoded.is_encoding_problem, problem);
            assert!(!decoded.selection.invalid_extended);
        }
    }
    #[test]
    fn unknown_empty_and_absent_labels_recover_without_new_word_placement() {
        for (source, expected, problem, rejected) in [
            (
                b"attachment;filename*=unknown''%E2%82%AC".as_slice(),
                "€",
                true,
                false,
            ),
            (b"attachment;filename*=''", "", true, false),
            (b"attachment;filename*=''data", "data", true, false),
            (b"attachment;filename*=unknown''%FF", "�", true, false),
            (
                b"attachment;filename*0=one;filename*1*=%20two",
                "one two",
                false,
                false,
            ),
            (
                b"attachment;filename=saved;filename*=utf-8''%xx",
                "saved",
                false,
                true,
            ),
            (b"attachment;filename*=utf-8''%E2", "�", true, false),
            (
                b"attachment;filename*0=one;filename*1=two",
                "onetwo",
                false,
                false,
            ),
            (
                b"attachment;filename*=utf-8''%3D%3Futf-8%3FQ%3Fx%3F%3D",
                "=?utf-8?Q?x?=",
                false,
                false,
            ),
            (
                b"attachment;filename=\"=?utf-8?Q?x?=\"",
                "=?utf-8?Q?x?=",
                false,
                false,
            ),
            (b"attachment;x=missing", "", false, false),
            (b"attachment;filename*=utf-8''%xx", "", false, true),
            (
                b"attachment;filename*0=plain;filename*1*=utf-8%27%27more",
                "plainutf-8''more",
                false,
                false,
            ),
            (
                b"attachment;filename=saved;filename*0=plain;filename*1*=utf-8''more",
                "saved",
                false,
                true,
            ),
        ] {
            let (out, decoded) = decode(source).unwrap();
            assert_eq!(out, expected);
            assert_eq!(decoded.is_encoding_problem, problem);
            assert_eq!(decoded.selection.invalid_extended, rejected);
        }
        assert_eq!(decode(b"attachment;filename=saved;"), Err(Error::Malformed));
        for attribute in [Attribute::Boundary, Attribute::Charset] {
            assert_eq!(
                Cursor::new(b"text/plain", Kind::ContentType, attribute).poll(Tick(1), &mut work()),
                Err(Error::InvalidState)
            );
        }
    }
    #[test]
    fn scalar_controls_are_retained_for_later_display_filtering() {
        let (out, decoded) =
            decode(b"attachment;filename*=utf-8''%00%01%7F%C2%80%EF%B7%90").unwrap();
        assert_eq!(out, "\0\u{1}\u{7f}\u{80}\u{fdd0}");
        assert!(!decoded.is_encoding_problem);
    }
    #[test]
    fn every_job_and_aggregate_cut_retires_scalar_derivation() {
        for source in [
            b"attachment;filename*1*=%82%AC;filename*0*=utf-8''%E2".as_slice(),
            b"attachment;filename*=unknown''%FF",
            b"attachment;filename=saved",
            b"attachment;x=missing",
        ] {
            let mut meter = work();
            let mut budget = crate::nfc::HeaderBudget::new();
            let before = (
                meter.remaining(),
                budget.source_bytes_remaining(),
                budget.steps_remaining(),
            );
            let mut full = Budgeted::new(
                source,
                Kind::ContentDisposition,
                Attribute::Filename,
                &mut meter,
                &mut budget,
            );
            assert!(std::mem::size_of_val(&full) <= 1312);
            let decoded = loop {
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
                if let Status::Complete(decoded) = status {
                    break decoded;
                }
            };
            let visits = before.1 - full.budget.source_bytes_remaining();
            let steps = before.2 - full.budget.steps_remaining();
            let records = before.0.records - full.work.remaining().records;
            let cached = (
                full.work.remaining(),
                full.budget.source_bytes_remaining(),
                full.budget.steps_remaining(),
                full.credit,
            );
            assert_eq!(full.poll(Tick(100)), Ok(Status::Complete(decoded)));
            assert_eq!(
                (
                    full.work.remaining(),
                    full.budget.source_bytes_remaining(),
                    full.budget.steps_remaining(),
                    full.credit
                ),
                cached
            );
            assert_eq!(
                full.check_deadline(Tick(100)),
                Err(Error::Work(Stop::Deadline))
            );
            assert_eq!(full.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
            for flavor in 0..4 {
                let amount = match flavor {
                    0 | 2 => visits,
                    1 => steps,
                    _ => records,
                };
                for limit in 0..amount {
                    let mut meter = if flavor >= 2 {
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
                            .charge(&mut meter, Tick(1), bytes, steps, &mut credit)
                            .unwrap();
                    }
                    let mut cursor = Budgeted::new(
                        source,
                        Kind::ContentDisposition,
                        Attribute::Filename,
                        &mut meter,
                        &mut budget,
                    );
                    loop {
                        match cursor.poll(Tick(1)) {
                            Ok(Status::Complete(_)) => panic!("cut completed scalar output"),
                            Ok(_) => {}
                            Err(error) => {
                                assert_eq!(
                                    error,
                                    match flavor {
                                        0 | 1 => Error::InterpretationLimit,
                                        2 => Error::Work(Stop::IoBytes),
                                        _ => Error::Work(Stop::Records),
                                    }
                                );
                                assert_eq!(cursor.poll(Tick(1)), Err(error));
                                assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn final_deadline_and_invalid_purpose_are_sticky() {
        for source in [
            b"attachment;filename*=unknown''%FF".as_slice(),
            b"attachment;x=missing",
        ] {
            let mut cursor = Cursor::new(source, Kind::ContentDisposition, Attribute::Filename);
            let mut meter = work();
            while !matches!(cursor.phase, Phase::Finish) {
                assert!(!matches!(
                    cursor.poll(Tick(1), &mut meter).unwrap(),
                    Status::Complete(_)
                ));
            }
            assert_eq!(
                cursor.poll(Tick(100), &mut meter),
                Err(Error::Work(Stop::Deadline))
            );
            assert_eq!(cursor.selection, None);
            let mut fresh = work();
            let before = fresh.remaining();
            assert_eq!(
                cursor.poll(Tick(1), &mut fresh),
                Err(Error::Work(Stop::Deadline))
            );
            assert_eq!(
                cursor.check_deadline(Tick(1), &mut fresh),
                Err(Error::Work(Stop::Deadline))
            );
            assert_eq!(fresh.remaining(), before);
        }
        let mut cursor = Cursor::new(b"text/plain", Kind::ContentType, Attribute::Boundary);
        let mut meter = work();
        assert_eq!(cursor.poll(Tick(1), &mut meter), Err(Error::InvalidState));
        assert_eq!(cursor.poll(Tick(1), &mut work()), Err(Error::InvalidState));
    }
    #[test]
    fn long_unicode_and_labels_keep_fixed_state_and_original_work() {
        let unicode = format!(
            "attachment;filename*0=\"{}\";filename*1=tail",
            "🐈".repeat(1024)
        );
        let label = format!("attachment;filename*= {}''value", "a".repeat(4096));
        for (source, expected, problem) in [
            (unicode.as_bytes(), 4100, false),
            (label.as_bytes(), 5, true),
        ] {
            let mut meter = work();
            let mut budget = crate::nfc::HeaderBudget::new();
            let mut cursor = Budgeted::new(
                source,
                Kind::ContentDisposition,
                Attribute::Filename,
                &mut meter,
                &mut budget,
            );
            let mut bytes = 0;
            let mut peak = 0;
            loop {
                let before = (
                    cursor.work.remaining(),
                    cursor.budget.source_bytes_remaining(),
                    cursor.budget.steps_remaining(),
                );
                let status = cursor.poll(Tick(1)).unwrap();
                let visits = before.1 - cursor.budget.source_bytes_remaining();
                peak = peak.max(visits);
                assert!(visits <= 160);
                assert!(before.0.io_bytes - cursor.work.remaining().io_bytes <= 160);
                assert!(before.0.records - cursor.work.remaining().records <= 16);
                assert!(before.2 - cursor.budget.steps_remaining() <= 256);
                assert_eq!(cursor.work.remaining().output_bytes, 0);
                match status {
                    Status::Yield => {}
                    Status::Scalar(value) => bytes += value.len_utf8(),
                    Status::Complete(decoded) => {
                        assert_eq!(decoded.is_encoding_problem, problem);
                        break;
                    }
                }
            }
            assert_eq!(bytes, expected);
            assert!(peak > 0);
            if !problem {
                assert_eq!(peak, 160);
            }
        }
    }
    #[test]
    fn every_alias_and_name_attribute_uses_the_shared_decoder_policy() {
        for (label, payload, expected) in [
            ("utf-8", "%E2%82%AC", "€"),
            ("utf8", "%E2%82%AC", "€"),
            ("us-ascii", "a", "a"),
            ("ascii", "a", "a"),
            ("ansi_x3.4-1968", "a", "a"),
            ("iso-8859-1", "%E9", "é"),
            ("latin1", "%E9", "é"),
            ("iso_8859-1", "%E9", "é"),
            ("windows-1252", "%80", "€"),
            ("cp1252", "%80", "€"),
        ] {
            for (kind, attribute, head, name) in [
                (Kind::ContentType, Attribute::Name, "text/plain", "name"),
                (
                    Kind::ContentDisposition,
                    Attribute::Filename,
                    "attachment",
                    "filename",
                ),
            ] {
                let source = format!("{head};{name}*={label}''{payload}");
                let (output, decoded) = decode_kind(source.as_bytes(), kind, attribute).unwrap();
                assert_eq!(output, expected);
                assert!(!decoded.is_encoding_problem);
                assert!(!decoded.selection.invalid_extended);
                assert!(matches!(decoded.selection.plan, Some(Plan::Extended(_))));
            }
        }
    }
    #[test]
    fn prefix_order_guards_and_initial_admission_precedence_are_explicit() {
        for role in [Role::Charset, Role::Language] {
            let mut cursor = Cursor::new(
                b"attachment;filename*=utf-8''ab",
                Kind::ContentDisposition,
                Attribute::Filename,
            );
            let mut meter = work();
            while !matches!(cursor.poll(Tick(1), &mut meter).unwrap(), Status::Scalar(_)) {}
            cursor.phase = Phase::Octet;
            cursor.held = Some((role, b'x'));
            assert_eq!(cursor.poll(Tick(1), &mut meter), Err(Error::InvalidState));
            assert_eq!(cursor.selection, None);
            assert_eq!(cursor.held, None);
            assert_eq!(cursor.poll(Tick(1), &mut work()), Err(Error::InvalidState));
            assert_eq!(
                cursor.check_deadline(Tick(1), &mut work()),
                Err(Error::InvalidState)
            );
        }
        let mut cursor = Cursor::new(b"text/plain", Kind::ContentType, Attribute::Boundary);
        let mut meter = Meter::new(Deadline::after(Tick(0), 100).unwrap(), Charge::default());
        assert_eq!(
            cursor.poll(Tick(1), &mut meter),
            Err(Error::Work(Stop::Records))
        );
        assert_eq!(
            cursor.poll(Tick(1), &mut work()),
            Err(Error::Work(Stop::Records))
        );
        let mut meter = work();
        let mut budget = crate::nfc::HeaderBudget::new();
        let mut credit = 0;
        let excess = budget.steps_remaining() + 1;
        assert_eq!(
            budget.charge(&mut meter, Tick(1), 0, excess, &mut credit),
            Err(crate::nfc::Error::InterpretationLimit)
        );
        let mut cursor = Budgeted::new(
            b"text/plain",
            Kind::ContentType,
            Attribute::Boundary,
            &mut meter,
            &mut budget,
        );
        assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
        assert_eq!(
            cursor.check_deadline(Tick(1)),
            Err(Error::InterpretationLimit)
        );
    }
}
