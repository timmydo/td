//! Validated MessageIds text conversion under the original email budgets.
use super::{Cursor, Error, Mode, Phase, Status};
use crate::{
    decode_work::{Conversion, Error as DecodeError},
    nfc::HeaderBudget,
    time::Tick,
    work::{Charge, Meter},
};
const UNFOLD_TRANSITIONS: usize = 127;
/// Retains exclusive budget borrows and non-replayable prepaid credit.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {}
/// cloned::<td_mime::header_message_ids::project::Budgeted<'_, '_>>();
/// ```
pub struct Budgeted<'a, 'w> {
    cursor: Cursor<'a>,
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    credit: u8,
    failure: Option<Error>,
}
impl<'a, 'w> Budgeted<'a, 'w> {
    pub fn new(
        source: &'a [u8],
        mode: Mode,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Self {
        Self::with_cursor(Cursor::new(source, mode), work, budget)
    }
    pub(crate) fn content_id(
        source: &'a [u8],
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Self {
        Self::with_cursor(Cursor::content_id(source), work, budget)
    }
    pub(crate) fn is_complete(&self) -> bool {
        self.failure.is_none() && matches!(self.cursor.phase, Phase::Complete)
    }
    pub(crate) fn addr_spec(
        source: &'a [u8],
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Self {
        Self::with_cursor(Cursor::addr_spec(source), work, budget)
    }
    pub(crate) fn fallback(
        source: &'a [u8],
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Self {
        Self::with_cursor(Cursor::fallback(source), work, budget)
    }
    fn with_cursor(cursor: Cursor<'a>, work: &'w mut Meter, budget: &'w mut HeaderBudget) -> Self {
        Self {
            cursor,
            work,
            budget,
            credit: 0,
            failure: None,
        }
    }
    #[cfg(test)]
    pub(crate) fn remaining(&self) -> (Charge, u64) {
        (self.work.remaining(), self.budget.steps_remaining())
    }
    pub(crate) fn finish(self) -> Result<(&'w mut Meter, &'w mut HeaderBudget), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !matches!(self.cursor.phase, Phase::Complete) {
            return Err(Error::InvalidState);
        }
        Ok((self.work, self.budget))
    }
    // Only whole-field syntax failure can become a null value.
    pub(crate) fn finish_malformed(self) -> Result<(&'w mut Meter, &'w mut HeaderBudget), Error> {
        if self.failure != Some(Error::Malformed) || !matches!(self.cursor.phase, Phase::Validate) {
            return Err(Error::InvalidState);
        }
        Ok((self.work, self.budget))
    }
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
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    /// Final only after Complete.
    pub const fn is_encoding_problem(&self) -> bool {
        self.cursor.is_encoding_problem()
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self
            .budget
            .charge_local(self.work, now, 0, 0, &mut self.credit)
            .map_err(DecodeError::from)
            .map_err(Error::from);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.cursor.phase, Phase::Complete) {
            return Ok(Status::Complete);
        }
        self.check_deadline(now)?;
        let result = self.cursor.poll_with_work::<UNFOLD_TRANSITIONS>(
            now,
            &mut Conversion::new(self.work, self.budget, &mut self.credit),
        );
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{decode_work::Work, time::Deadline, work::Stop};
    fn work() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 2_000_000,
                output_bytes: 1_000_000,
                ..Charge::default()
            },
        )
    }
    fn limited(bytes: u64, steps: u64) -> HeaderBudget {
        let mut budget = HeaderBudget::new();
        budget
            .charge_local(
                &mut work(),
                Tick(1),
                budget.source_bytes_remaining() - bytes,
                budget.steps_remaining() - steps,
                &mut 0,
            )
            .unwrap();
        budget
    }
    fn drain(cursor: &mut Budgeted<'_, '_>) -> (Vec<Status>, Result<(), Error>) {
        assert!(std::mem::size_of_val(cursor) <= 416);
        let mut events = Vec::new();
        for _ in 0..1_000_000 {
            let before = cursor.work.remaining();
            let steps = cursor.budget.steps_remaining();
            let status = cursor.poll(Tick(1));
            let after = cursor.work.remaining();
            assert!(before.io_bytes - after.io_bytes <= 160);
            assert!(before.records - after.records <= 16);
            assert!(steps - cursor.budget.steps_remaining() <= 255);
            assert!(before.output_bytes - after.output_bytes <= 4);
            match status {
                Err(error) => return (events, Err(error)),
                Ok(Status::Yield) => {}
                Ok(Status::Complete) => {
                    assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                    assert_eq!(cursor.work.remaining(), after);
                    return (events, Ok(()));
                }
                Ok(event) => events.push(event),
            }
        }
        panic!("budgeted MessageIds conversion did not finish");
    }
    #[test]
    fn converted_values_preserve_plain_events_visits_and_output_charges() {
        let long = format!("({0})<{0}@b><\"{0}\"@[{0}]>", "🐈".repeat(4096));
        let nested = format!("<a@b>{}", "(".repeat(33));
        for source in [
            b"".as_slice(),
            b"<a@b>",
            b"old <a@b> tail",
            b"<a@b><bad>",
            b"<a@b> (bad",
            b"<\"a\r\n b\"@[c\n\td]>",
            b"<\"\\\0\"@[]>",
            "<e\u{301}@例>".as_bytes(),
            "<\u{fdd0}@\u{10ffff}>".as_bytes(),
            long.as_bytes(),
            nested.as_bytes(),
        ] {
            for mode in [Mode::Strict, Mode::ObsoletePhrases] {
                let mut plain_work = work();
                let mut plain = Cursor::new(source, mode);
                let mut events = Vec::new();
                let result = loop {
                    match plain.poll(Tick(1), &mut plain_work) {
                        Ok(Status::Yield) => {}
                        Ok(Status::Complete) => break Ok(()),
                        Err(error) => break Err(error),
                        Ok(event) => events.push(event),
                    }
                };
                let mut work = work();
                let mut budget = HeaderBudget::new();
                let mut cursor = Budgeted::new(source, mode, &mut work, &mut budget);
                assert_eq!(drain(&mut cursor), (events, result));
                assert_eq!(cursor.is_encoding_problem(), plain.is_encoding_problem());
                assert_eq!(work.remaining().io_bytes, plain_work.remaining().io_bytes);
                assert_eq!(
                    work.remaining().output_bytes,
                    plain_work.remaining().output_bytes
                );
                assert_eq!(
                    16 * 1024 * 1024 - budget.source_bytes_remaining(),
                    100_000_000 - work.remaining().io_bytes
                );
                assert_eq!(
                    2_000_000 - work.remaining().records,
                    (16_000_000 - budget.steps_remaining()).div_ceil(16)
                );
                if result.is_err() {
                    assert_eq!(work.remaining().output_bytes, 1_000_000);
                }
            }
        }
    }
    #[test]
    fn partial_budgets_retire_validation_and_replay_without_accepting_prefixes() {
        let mut late = false;
        for source in [
            b"<a@b>".as_slice(),
            b"<a@b> (bad",
            b"<\xe2\x82",
            b"<\"a\r\n b\"@[c]>",
            "<\u{fdd0}@b>".as_bytes(),
            b"old <a@b> tail",
        ] {
            let mut work = work();
            let mut budget = HeaderBudget::new();
            let expected = drain(&mut Budgeted::new(
                source,
                Mode::ObsoletePhrases,
                &mut work,
                &mut budget,
            ));
            let steps = 16_000_000 - budget.steps_remaining();
            let visits = 16 * 1024 * 1024 - budget.source_bytes_remaining();
            for (bytes, steps) in (0..steps)
                .map(|cut| (visits, cut))
                .chain((0..visits).map(|cut| (cut, steps)))
            {
                let mut work = self::work();
                let mut budget = limited(bytes, steps);
                let mut cursor =
                    Budgeted::new(source, Mode::ObsoletePhrases, &mut work, &mut budget);
                let (events, result) = drain(&mut cursor);
                assert_eq!(result, Err(Error::InterpretationLimit));
                assert!(expected.0.starts_with(&events));
                late |= events.contains(&Status::End);
                assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
                assert_eq!(work.stopped(), None);
            }
            let mut work = self::work();
            let mut budget = limited(visits, steps);
            assert_eq!(
                drain(&mut Budgeted::new(
                    source,
                    Mode::ObsoletePhrases,
                    &mut work,
                    &mut budget
                )),
                expected
            );
        }
        assert!(late);
    }
    #[test]
    fn field_credit_and_final_admission_cannot_be_replayed() {
        let mut work = work();
        let mut budget = limited(0, 30);
        for steps in [20, 10, 0] {
            assert_eq!(
                drain(&mut Budgeted::new(
                    b"",
                    Mode::ObsoletePhrases,
                    &mut work,
                    &mut budget
                )),
                (Vec::new(), Ok(()))
            );
            assert_eq!(budget.steps_remaining(), steps);
        }
        assert_eq!(work.remaining().records, 1_999_997);
        assert_eq!(
            Budgeted::new(b"", Mode::ObsoletePhrases, &mut work, &mut budget).poll(Tick(1)),
            Err(Error::InterpretationLimit)
        );
        let mut budget = HeaderBudget::new();
        let mut cursor = Budgeted::new(b"<a@b>", Mode::Strict, &mut work, &mut budget);
        assert_eq!(drain(&mut cursor).1, Ok(()));
        let before = cursor.work.remaining();
        cursor.check_deadline(Tick(1)).unwrap();
        assert_eq!(cursor.work.remaining(), before);
        assert_eq!(
            cursor.check_deadline(Tick(100)),
            Err(Error::Work(Stop::Deadline))
        );
        assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
    }
    #[test]
    fn output_limits_preserve_typed_job_failure_and_retire_parent() {
        for output in 0..6 {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: 1000,
                    records: 1000,
                    output_bytes: output,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            let mut cursor = Budgeted::new(b"<a@b>", Mode::Strict, &mut work, &mut budget);
            assert_eq!(drain(&mut cursor).1, Err(Error::Work(Stop::OutputBytes)));
            let after = cursor.work.remaining();
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::OutputBytes)));
            assert_eq!(cursor.work.remaining(), after);
            assert_eq!(work.stopped(), Some(Stop::OutputBytes));
            budget
                .charge_local(&mut self::work(), Tick(1), 0, 0, &mut 0)
                .unwrap();
        }
    }
    #[test]
    fn conversion_output_refusal_keeps_only_previously_admitted_work() {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                records: 10,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut credit = 0;
        let invalid = Charge {
            io_bytes: 1,
            output_bytes: 1,
            ..Charge::default()
        };
        let before = work.remaining();
        assert_eq!(
            Conversion::new(&mut work, &mut budget, &mut credit).charge(Tick(1), invalid),
            Err(DecodeError::InvalidState)
        );
        assert_eq!(work.remaining(), before);
        assert_eq!(budget.steps_remaining(), 16_000_000);
        assert_eq!(credit, 0);
        assert_eq!(
            Conversion::new(&mut work, &mut budget, &mut credit).charge(
                Tick(1),
                Charge {
                    output_bytes: 1,
                    ..Charge::default()
                }
            ),
            Err(DecodeError::Work(Stop::OutputBytes))
        );
        assert_eq!(budget.steps_remaining(), 15_999_999);
        assert_eq!(work.remaining().records, 9);
        assert_eq!(work.remaining().output_bytes, 0);
        assert_eq!(credit, 15);
    }
    #[test]
    fn exact_nonempty_conversion_includes_eof_and_output_steps() {
        let mut work = work();
        let mut budget = HeaderBudget::new();
        assert_eq!(
            drain(&mut Budgeted::new(
                b"<a@b>",
                Mode::Strict,
                &mut work,
                &mut budget
            ))
            .1,
            Ok(())
        );
        assert_eq!(100_000_000 - work.remaining().io_bytes, 34);
        assert_eq!(16_000_000 - budget.steps_remaining(), 129);
        assert_eq!(2_000_000 - work.remaining().records, 9);
        assert_eq!(1_000_000 - work.remaining().output_bytes, 6);
    }
    #[test]
    fn private_unfold_cap_eof_and_backpressure_pin_transition_charges() {
        use crate::unfold::{Decoder, Error as UnfoldError, Status as Unfolded};
        for (input, last, capacity, consumed, written, status, steps) in [
            (&[b'x'; 4096][..], true, 4096, 64, 63, Unfolded::Yield, 254),
            (b"a".as_slice(), true, 0, 1, 0, Unfolded::NeedOutput, 3),
            (b"a", true, 1, 1, 1, Unfolded::Complete, 6),
            (b"", true, 0, 0, 0, Unfolded::Complete, 2),
            (b"", false, 0, 0, 0, Unfolded::NeedInput, 1),
        ] {
            let mut work = work();
            let mut budget = HeaderBudget::new();
            let mut credit = 0;
            let mut output = [0xa5; 4096];
            let progress = Decoder::default()
                .poll_with_work::<UNFOLD_TRANSITIONS>(
                    input,
                    &mut output[..capacity],
                    last,
                    Tick(1),
                    &mut Conversion::new(&mut work, &mut budget, &mut credit),
                )
                .unwrap();
            assert_eq!(
                (progress.consumed, progress.written, progress.status),
                (consumed, written, status)
            );
            assert_eq!(16_000_000 - budget.steps_remaining(), steps);
            assert_eq!(100_000_000 - work.remaining().io_bytes, consumed as u64);
            assert_eq!(1_000_000 - work.remaining().output_bytes, written as u64);
            assert!(output[written..].iter().all(|byte| *byte == 0xa5));
        }
        for limit in [0, 257] {
            let mut work = work();
            let before = work.remaining();
            let mut decoder = Decoder::default();
            let mut output = [0xa5; 1];
            let result = if limit == 0 {
                decoder.poll_with_work::<0>(b"a", &mut output, true, Tick(1), &mut work)
            } else {
                decoder.poll_with_work::<257>(b"a", &mut output, true, Tick(1), &mut work)
            };
            assert_eq!(result, Err(UnfoldError::InvalidState));
            assert_eq!(
                decoder.poll(b"a", &mut output, true, Tick(1), &mut work),
                result
            );
            assert_eq!(output, [0xa5]);
            assert_eq!(work.remaining(), before);
        }
    }
}
