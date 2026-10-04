//! Provisional display scalars from original MIME parameter source; NFC is external.
pub mod normalized;
mod ordinary;
use super::{scalars, Attribute, Error, Plan};
use crate::{
    admission::work::{Charge, Meter},
    decode_work::{self, Work},
    mime_fields::Kind,
    ports::Tick,
};
pub use scalars::{Decoded, Status};
fn filter(value: char, problem: &mut bool) -> Option<char> {
    if value == '\0' {
        return None;
    }
    if crate::unicode::is_noncharacter(value) {
        *problem = true;
        Some('\u{fffd}')
    } else {
        Some(value)
    }
}
// Keep the active parser inline in the fixed reservation.
#[allow(clippy::large_enum_variant)]
enum Phase<'a> {
    Literal(scalars::Cursor<'a>),
    Ordinary(ordinary::Cursor<'a>),
    Finish,
    Complete,
    Failed,
}
/// Original-source compatibility/filtering; output remains provisional.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_parameter::display::Cursor<'_>>();
/// ```
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_parameter::display::Cursor<'_>>();
/// ```
pub struct Cursor<'a> {
    source: &'a [u8],
    phase: Phase<'a>,
    result: Option<Decoded>,
    problem: bool,
    failure: Option<Error>,
}
// Pure replay state stays inline within the parser reservation.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy)]
enum CheckpointPhase<'a> {
    Literal(scalars::Checkpoint<'a>),
    Ordinary(ordinary::Checkpoint<'a>),
    Finish,
    Complete,
}
#[derive(Clone, Copy)]
struct Checkpoint<'a> {
    source: &'a [u8],
    phase: CheckpointPhase<'a>,
    result: Option<Decoded>,
    problem: bool,
}
impl<'a> Cursor<'a> {
    fn checkpoint(&self) -> Result<Checkpoint<'a>, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        Ok(Checkpoint {
            source: self.source,
            phase: match &self.phase {
                Phase::Literal(cursor) => CheckpointPhase::Literal(cursor.checkpoint()?),
                Phase::Ordinary(cursor) => CheckpointPhase::Ordinary(cursor.checkpoint()),
                Phase::Finish => CheckpointPhase::Finish,
                Phase::Complete => CheckpointPhase::Complete,
                Phase::Failed => return Err(Error::InvalidState),
            },
            result: self.result,
            problem: self.problem,
        })
    }
    fn resume(checkpoint: Checkpoint<'a>) -> Self {
        Self {
            source: checkpoint.source,
            phase: match checkpoint.phase {
                CheckpointPhase::Literal(progress) => {
                    Phase::Literal(scalars::Cursor::resume(progress))
                }
                CheckpointPhase::Ordinary(progress) => {
                    Phase::Ordinary(ordinary::Cursor::resume(progress))
                }
                CheckpointPhase::Finish => Phase::Finish,
                CheckpointPhase::Complete => Phase::Complete,
            },
            result: checkpoint.result,
            problem: checkpoint.problem,
            failure: None,
        }
    }

    #[must_use]
    pub const fn new(source: &'a [u8], kind: Kind, attribute: Attribute) -> Self {
        Self {
            source,
            phase: Phase::Literal(scalars::Cursor::new(source, kind, attribute)),
            result: None,
            problem: false,
            failure: None,
        }
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
            self.result = None;
            self.phase = Phase::Failed;
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick, work: &mut Meter) -> Result<(), Error> {
        self.check(|| Work::charge(work, now, Charge::default()))
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        self.poll_with_work(now, work)
    }
    fn poll_with_work(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete(self.result.ok_or(Error::InvalidState)?));
        }
        let result = self.step(now, work);
        if let Err(error) = result {
            self.failure = Some(error);
            self.result = None;
            self.phase = Phase::Failed;
        }
        result
    }
    fn step(&mut self, now: Tick, work: &mut impl Work) -> Result<Status, Error> {
        work.charge(
            now,
            Charge {
                records: 1,
                ..Charge::default()
            },
        )?;
        match &mut self.phase {
            Phase::Literal(cursor) => {
                let status = cursor.poll_with_work(now, work)?;
                match status {
                    Status::Yield => {
                        if let Some(selection) = cursor.ordinary_ready() {
                            let Some(Plan::Ordinary(parameter)) = selection.plan else {
                                return Err(Error::InvalidState);
                            };
                            self.result = Some(Decoded {
                                selection,
                                is_encoding_problem: false,
                            });
                            self.phase =
                                Phase::Ordinary(ordinary::Cursor::new(self.source, parameter)?);
                        }
                    }
                    Status::Scalar(value) => {
                        work.charge(
                            now,
                            Charge {
                                records: 1,
                                ..Charge::default()
                            },
                        )?;
                        return Ok(
                            filter(value, &mut self.problem).map_or(Status::Yield, Status::Scalar)
                        );
                    }
                    Status::Complete(mut decoded) => {
                        decoded.is_encoding_problem |= self.problem;
                        self.result = Some(decoded);
                        self.phase = Phase::Finish;
                    }
                }
            }
            Phase::Ordinary(cursor) => match cursor.poll(now, work)? {
                crate::encoded_word::decode::Status::Yield => {}
                crate::encoded_word::decode::Status::Scalar(value) => {
                    return Ok(Status::Scalar(value))
                }
                crate::encoded_word::decode::Status::Complete => {
                    self.result
                        .as_mut()
                        .ok_or(Error::InvalidState)?
                        .is_encoding_problem |= cursor.is_encoding_problem();
                    self.phase = Phase::Finish;
                }
            },
            Phase::Finish => {
                let decoded = self.result.ok_or(Error::InvalidState)?;
                self.phase = Phase::Complete;
                return Ok(Status::Complete(decoded));
            }
            Phase::Complete | Phase::Failed => return Err(Error::InvalidState),
        }
        Ok(Status::Yield)
    }
}
/// Same original allowances and credit across literal and compatibility branches.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_parameter::display::Budgeted<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_parameter::display::Budgeted<'_, '_>>();
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
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{
        admission::work::{Charge, Meter, Stop},
        encoded_word::decode::Status as WordStatus,
        mime_fields::{Extent, Parameter},
        ports::{Deadline, Tick},
    };
    fn decode(raw: &[u8]) -> (String, bool) {
        assert!(std::mem::size_of::<ordinary::Cursor<'_>>() <= 256);
        let mut c = ordinary::Cursor::new(
            raw,
            Parameter {
                name: Extent { start: 0, end: 0 },
                value: Extent {
                    start: 0,
                    end: raw.len(),
                },
                quoted: raw.first() == Some(&b'"'),
            },
        )
        .unwrap();
        let mut m = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 10_000_000,
                records: 10_000_000,
                ..Charge::default()
            },
        );
        let mut out = String::new();
        loop {
            let before = m.remaining();
            let s = c.poll(Tick(1), &mut m).unwrap();
            assert!(before.io_bytes - m.remaining().io_bytes <= 225);
            assert!(before.records - m.remaining().records <= 227);
            match s {
                WordStatus::Scalar(c) => out.push(c),
                WordStatus::Yield => {}
                WordStatus::Complete => return (out, c.is_encoding_problem()),
            }
        }
    }

    #[test]
    fn pure_checkpoints_preserve_every_display_event_and_charge_trace() {
        for source in [
            b"attachment;filename=\"=?utf-8?Q?e?= =?utf-8?Q?=CC=81?=\"".as_slice(),
            b"attachment;filename*1*=%81;filename*0*=utf-8''e%CC",
            b"attachment;filename=saved;filename*=utf-8''%xx",
            b"attachment;x=missing",
            b"attachment;filename=first;filename=second;filename*=utf-8'en'%xx",
        ] {
            let mut cursor = Cursor::new(source, Kind::ContentDisposition, Attribute::Filename);
            let mut completed = false;
            for _ in 0..10000 {
                let snapshot = cursor.checkpoint().unwrap();
                let mut original = work();
                let before = original.remaining();
                let event = cursor.poll(Tick(1), &mut original).unwrap();
                let mut restored = Cursor::resume(snapshot);
                let mut replay = work();
                assert_eq!(restored.poll(Tick(1), &mut replay), Ok(event));
                assert_eq!(replay.remaining(), original.remaining());

                let mut reference_work = work();
                let mut reference_budget = crate::nfc::HeaderBudget::new();
                let reference_start = (
                    reference_budget.source_bytes_remaining(),
                    reference_budget.steps_remaining(),
                );
                assert_eq!(
                    Cursor::resume(snapshot).poll_with_work(
                        Tick(1),
                        &mut decode_work::Parsing::new(
                            &mut reference_work,
                            &mut reference_budget,
                            &mut 0
                        )
                    ),
                    Ok(event)
                );
                let reference_cost = (
                    reference_start.0 - reference_budget.source_bytes_remaining(),
                    reference_start.1 - reference_budget.steps_remaining(),
                );
                for mut credit in [0, 1, 15] {
                    let mut replay = work();
                    let mut budget = crate::nfc::HeaderBudget::new();
                    budget.charge(&mut replay, Tick(1), 21, 37, &mut 0).unwrap();
                    let before_steps = budget.steps_remaining();
                    let before_bytes = budget.source_bytes_remaining();
                    let before_records = replay.remaining().records;
                    let initial_credit = u64::from(credit);
                    let mut restored = Cursor::resume(snapshot);
                    assert_eq!(
                        restored.poll_with_work(
                            Tick(1),
                            &mut decode_work::Parsing::new(&mut replay, &mut budget, &mut credit)
                        ),
                        Ok(event)
                    );
                    assert_eq!(
                        before_bytes - budget.source_bytes_remaining(),
                        reference_cost.0
                    );
                    assert_eq!(before_steps - budget.steps_remaining(), reference_cost.1);
                    let records = reference_cost.1.saturating_sub(initial_credit).div_ceil(16);
                    assert_eq!(before_records - replay.remaining().records, records);
                    assert_eq!(
                        u64::from(credit),
                        initial_credit + records * 16 - reference_cost.1
                    );
                }
                let visits = before.io_bytes - original.remaining().io_bytes;
                let records = before.records - original.remaining().records;
                for flavor in 0..2 {
                    for amount in 0..if flavor == 0 { visits } else { records } {
                        let mut cut = Meter::new(
                            Deadline::after(Tick(0), 100).unwrap(),
                            Charge {
                                io_bytes: if flavor == 0 { amount } else { visits },
                                records: if flavor == 1 { amount } else { records },
                                ..Charge::default()
                            },
                        );
                        let mut failed = Cursor::resume(snapshot);
                        let error = failed.poll(Tick(1), &mut cut).err().unwrap();
                        assert_eq!(failed.checkpoint().err(), Some(error));
                        let mut fresh = work();
                        let prior = fresh.remaining();
                        assert_eq!(failed.poll(Tick(1), &mut fresh), Err(error));
                        assert_eq!(failed.check_deadline(Tick(1), &mut fresh), Err(error));
                        assert_eq!(fresh.remaining(), prior);
                    }
                }
                if matches!(event, Status::Complete(_)) {
                    completed = true;
                    break;
                }
            }
            assert!(completed);
        }
    }
    #[test]
    fn original_raw_compatibility_and_literal_filtering() {
        for (raw, want, problem) in [
            (
                b"\"=?utf-8?Q?hello_world?=\"".as_slice(),
                "hello world",
                false,
            ),
            (
                b"\" =?utf-8?Q?a?=\r\n \t=?utf-8?Q?b?= \t\"",
                " ab \t",
                false,
            ),
            (
                b"\"=?utf-8?Q?a?= =?unknown?Q?b?=\"",
                "a =?unknown?Q?b?=",
                false,
            ),
            (b"\"x=?utf-8?Q?a?=\"", "x=?utf-8?Q?a?=", false),
            (b"\"=?utf-8?Q?a?=x\"", "=?utf-8?Q?a?=x", false),
            (b"\"\\=?utf-8?Q?a?=\"", "=?utf-8?Q?a?=", false),
            (b"\"x\\ =?utf-8?Q?a?=\"", "x =?utf-8?Q?a?=", false),
            (b"\"\\\r\n =?utf-8?Q?a?=\"", " =?utf-8?Q?a?=", false),
            (b"\"x\\\r\\\n =?utf-8?Q?a?=\"", "x =?utf-8?Q?a?=", false),
            (
                b"\"=?utf-8?Q?a?= \\\r\n =?utf-8?Q?b?=\"",
                "a  =?utf-8?Q?b?=",
                false,
            ),
            (b"\"=?utf-8?Q?=00?= \t=?utf-8?Q?b?=\"", "b", false),
            (b"\"=?utf-8?Q?\\a?=\"", "=?utf-8?Q?a?=", false),
            (
                b"\"=?utf-8?Q?a?=\\ =?utf-8?Q?b?=\"",
                "=?utf-8?Q?a?= =?utf-8?Q?b?=",
                false,
            ),
            (b"\"=?utf-8?Q?=00=01=7F=C2=80=EF=B7=90?=\"", "�", true),
            (b"\"a\\\0\x01\x7fb\xef\xb7\x90\"", "a\u{1}\u{7f}b�", true),
            (b"\"=?utf-8?Q?=E2?= =?utf-8?Q?=82=AC?=\"", "���", true),
            (
                b"\"=?utf-8?Q?a?==?utf-8?Q?b?=\"",
                "=?utf-8?Q?a?==?utf-8?Q?b?=",
                false,
            ),
        ] {
            assert_eq!(decode(raw), (want.to_owned(), problem), "{raw:?}");
            let mut field = b"attachment;filename=".to_vec();
            field.extend_from_slice(raw);
            let (output, decoded) =
                display(&field, Kind::ContentDisposition, Attribute::Filename).unwrap();
            assert_eq!(output, want);
            assert_eq!(decoded.is_encoding_problem, problem);
            assert!(!decoded.selection.invalid_extended);
            assert!(matches!(decoded.selection.plan, Some(Plan::Ordinary(_))));
        }
    }
    #[test]
    fn complete_display_binding_and_family_diagnostics() {
        for (source, want, problem, rejected) in [
            (
                b"attachment;filename=\"=?utf-8?Q?hello_world?=\"".as_slice(),
                "hello world",
                false,
                false,
            ),
            (
                b"attachment;filename=\"x\\ =?utf-8?Q?a?=\"",
                "x =?utf-8?Q?a?=",
                false,
                false,
            ),
            (
                b"attachment;filename=saved;filename*=utf-8''%xx",
                "saved",
                false,
                true,
            ),
            (
                b"attachment;filename*=utf-8''%3D%3Futf-8%3FQ%3Fx%3F%3D",
                "=?utf-8?Q?x?=",
                false,
                false,
            ),
            (
                b"attachment;filename*0=\"=?utf-8?Q?\";filename*1=\"x?=\"",
                "=?utf-8?Q?x?=",
                false,
                false,
            ),
            (
                b"attachment;filename*=unknown''%00%01%7F%C2%80%EF%B7%90",
                "\u{1}\u{7f}\u{80}�",
                true,
                false,
            ),
            (b"attachment;filename=\"\"", "", false, false),
            (b"attachment;filename*=utf-8''", "", false, false),
            (b"attachment;filename=\"=?utf-8?Q?=00?=\"", "", false, false),
            (b"attachment;filename=\"  \"", "  ", false, false),
            (b"attachment;filename=\"a\\\0b\"", "ab", false, false),
            (
                b"attachment;filename=\"\\\xc3\xa9 =?utf-8?Q?a?=\"",
                "é a",
                false,
                false,
            ),
            (
                b"attachment;filename=\"=?utf-8?B?4oKs?=\"",
                "€",
                false,
                false,
            ),
            (b"attachment;x=missing", "", false, false),
        ] {
            let mut meter = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: 100_000_000,
                    records: 100_000_000,
                    ..Charge::default()
                },
            );
            let mut budget = crate::nfc::HeaderBudget::new();
            let mut cursor = Budgeted::new(
                source,
                Kind::ContentDisposition,
                Attribute::Filename,
                &mut meter,
                &mut budget,
            );
            assert!(std::mem::size_of_val(&cursor) <= 1536);
            let mut output = String::new();
            loop {
                let before = (
                    cursor.work.remaining(),
                    cursor.budget.source_bytes_remaining(),
                    cursor.budget.steps_remaining(),
                );
                let status = cursor.poll(Tick(1)).unwrap();
                assert!(before.0.io_bytes - cursor.work.remaining().io_bytes <= 225);
                assert!(before.0.records - cursor.work.remaining().records <= 29);
                assert!(before.1 - cursor.budget.source_bytes_remaining() <= 225);
                assert!(before.2 - cursor.budget.steps_remaining() <= 453);
                match status {
                    Status::Yield => {}
                    Status::Scalar(value) => output.push(value),
                    Status::Complete(decoded) => {
                        assert_eq!(output, want);
                        assert_eq!(decoded.is_encoding_problem, problem);
                        assert_eq!(decoded.selection.invalid_extended, rejected);
                        assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete(decoded)));
                        assert_eq!(
                            cursor.check_deadline(Tick(100)),
                            Err(Error::Work(crate::admission::work::Stop::Deadline))
                        );
                        break;
                    }
                }
            }
        }
    }
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
    fn display(
        source: &[u8],
        kind: Kind,
        attribute: Attribute,
    ) -> Result<(String, Decoded), Error> {
        let mut cursor = Cursor::new(source, kind, attribute);
        assert!(std::mem::size_of_val(&cursor) <= 1504);
        let mut meter = work();
        let mut output = String::new();
        loop {
            let before = meter.remaining();
            let status = cursor.poll(Tick(1), &mut meter)?;
            assert!(before.io_bytes - meter.remaining().io_bytes <= 225);
            assert!(before.records - meter.remaining().records <= 228);
            match status {
                Status::Yield => {}
                Status::Scalar(value) => output.push(value),
                Status::Complete(decoded) => return Ok((output, decoded)),
            }
        }
    }
    #[test]
    fn complete_binding_prevents_early_words_and_manufactured_placement() {
        for (source, want, problem, rejected) in [
            (
                b"attachment;filename=\"=?utf-8?Q?a?= =?utf-8?Q?b?=\";filename*=utf-8''%xx"
                    .as_slice(),
                "ab",
                false,
                true,
            ),
            (
                b"attachment;filename*0=\"=?utf-8?Q?\";filename*1=\"x?=\"",
                "=?utf-8?Q?x?=",
                false,
                false,
            ),
            (
                b"attachment;filename*0=\"=?utf-8?Q?x?=\";filename*1=tail",
                "=?utf-8?Q?x?=tail",
                false,
                false,
            ),
            (b"attachment;filename*=utf-8''%E1%00%80", "��", true, false),
            (
                b"attachment;filename*=utf-8''%00%01%7F%C2%80%EF%B7%90",
                "\u{1}\u{7f}\u{80}�",
                true,
                false,
            ),
        ] {
            let (output, decoded) =
                display(source, Kind::ContentDisposition, Attribute::Filename).unwrap();
            assert_eq!(output, want);
            assert_eq!(decoded.is_encoding_problem, problem);
            assert_eq!(decoded.selection.invalid_extended, rejected);
        }
        let mut cursor = Cursor::new(
            b"attachment;filename=\"=?utf-8?Q?x?=\";",
            Kind::ContentDisposition,
            Attribute::Filename,
        );
        let mut meter = work();
        loop {
            match cursor.poll(Tick(1), &mut meter) {
                Ok(Status::Yield) => {}
                Ok(_) => panic!("whole-field failure leaked display"),
                Err(error) => {
                    assert_eq!(error, Error::Malformed);
                    assert_eq!(cursor.poll(Tick(1), &mut work()), Err(error));
                    break;
                }
            }
        }
        let (output, decoded) = display(
            b"text/plain;name=\"=?latin1?Q?caf=E9?=\"",
            Kind::ContentType,
            Attribute::Name,
        )
        .unwrap();
        assert_eq!(output, "café");
        assert!(!decoded.is_encoding_problem);
        assert!(matches!(decoded.selection.plan, Some(Plan::Ordinary(_))));
    }
    #[test]
    fn maximal_recognition_and_long_literal_conversion_remain_bounded() {
        let max = format!("attachment;filename=\"=?utf-8?Q?{}?=\"", "a".repeat(63));
        let long = format!("attachment;filename=\"{}\"", "🐈".repeat(1024));
        let over = format!("attachment;filename=\"=?utf-8?Q?{}?=\"", "a".repeat(64));
        let mut plain = Cursor::new(
            max.as_bytes(),
            Kind::ContentDisposition,
            Attribute::Filename,
        );
        let mut meter = work();
        let mut peak_records = 0;
        let mut peak_visits = 0;
        let mut bytes = 0;
        loop {
            let before = meter.remaining();
            let status = plain.poll(Tick(1), &mut meter).unwrap();
            peak_records = peak_records.max(before.records - meter.remaining().records);
            peak_visits = peak_visits.max(before.io_bytes - meter.remaining().io_bytes);
            match status {
                Status::Yield => {}
                Status::Scalar(value) => bytes += value.len_utf8(),
                Status::Complete(decoded) => {
                    assert!(!decoded.is_encoding_problem);
                    break;
                }
            }
        }
        assert_eq!(bytes, 63);
        assert_eq!(peak_records, 228);
        assert_eq!(peak_visits, 225);
        for (source, want_bytes, maximal) in [
            (max.as_bytes(), 63, true),
            (long.as_bytes(), 4096, false),
            (over.as_bytes(), 76, false),
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
            let mut peak_visits = 0;
            let mut peak_steps = 0;
            let mut bytes = 0;
            loop {
                let before = (
                    cursor.work.remaining(),
                    cursor.budget.source_bytes_remaining(),
                    cursor.budget.steps_remaining(),
                );
                let status = cursor.poll(Tick(1)).unwrap();
                let visits = before.1 - cursor.budget.source_bytes_remaining();
                let steps = before.2 - cursor.budget.steps_remaining();
                peak_visits = peak_visits.max(visits);
                peak_steps = peak_steps.max(steps);
                assert!(visits <= 225);
                assert!(steps <= 453);
                assert!(before.0.records - cursor.work.remaining().records <= 29);
                assert_eq!(cursor.work.remaining().output_bytes, 0);
                match status {
                    Status::Yield => {}
                    Status::Scalar(value) => bytes += value.len_utf8(),
                    Status::Complete(decoded) => {
                        assert!(!decoded.is_encoding_problem);
                        break;
                    }
                }
            }
            assert_eq!(bytes, want_bytes);
            if maximal {
                assert_eq!(peak_visits, 225);
                assert_eq!(peak_steps, 453);
            }
        }
    }
    #[test]
    fn every_job_and_aggregate_cut_retires_display_derivation() {
        for source in [
            b"attachment;filename*1*=%82%AC;filename*0*=utf-8''%E2".as_slice(),
            b"attachment;filename*=unknown''%FF",
            b"attachment;filename=\"=?utf-8?Q?a?= =?unknown?Q?b?=\"",
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
            assert!(std::mem::size_of_val(&full) <= 1536);
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
                assert!(turn.0.io_bytes - full.work.remaining().io_bytes <= 225);
                assert!(turn.0.records - full.work.remaining().records <= 29);
                assert!(turn.1 - full.budget.source_bytes_remaining() <= 225);
                assert!(turn.2 - full.budget.steps_remaining() <= 453);
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
                            Ok(Status::Complete(_)) => panic!("cut completed display output"),
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
    fn display_final_deadline_and_invalid_purpose_are_sticky() {
        for source in [
            b"attachment;filename*=unknown''%FF".as_slice(),
            b"attachment;filename=\"=?utf-8?Q?x?=\"",
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
            assert_eq!(cursor.result, None);
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
}
