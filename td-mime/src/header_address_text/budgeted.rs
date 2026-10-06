//! Parsed/fallback address text under the original job and email budgets.
use super::{Error, Mode, Status};
use crate::{header_message_ids::project, nfc::HeaderBudget, time::Tick, work::Meter};
/// Retains the original budgets; text and diagnostics remain provisional.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {}
/// cloned::<td_mime::header_address_text::Budgeted<'_, '_>>();
/// ```
pub struct Budgeted<'a, 'w> {
    inner: project::Budgeted<'a, 'w>,
    failure: Option<Error>,
}
impl<'a, 'w> Budgeted<'a, 'w> {
    #[cfg(test)]
    pub(crate) fn remaining(&self) -> (crate::work::Charge, u64) {
        self.inner.remaining()
    }

    pub fn new(
        source: &'a [u8],
        mode: Mode,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Self {
        let inner = match mode {
            Mode::Parsed => project::Budgeted::addr_spec(source, work, budget),
            Mode::Fallback => project::Budgeted::fallback(source, work, budget),
        };
        Self {
            inner,
            failure: None,
        }
    }
    /// Final only after Complete.
    pub const fn is_encoding_problem(&self) -> bool {
        self.inner.is_encoding_problem()
    }
    pub(crate) fn finish(self) -> Result<(&'w mut Meter, &'w mut HeaderBudget), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.inner.finish().map_err(Error::from)
    }
    pub(crate) fn charge_output(&mut self, now: Tick, bytes: u64) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.inner.charge_output(now, bytes).map_err(Error::from);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.inner.check_deadline(now).map_err(Error::from);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = match self.inner.poll(now) {
            Ok(project::Status::Yield) => Ok(Status::Yield),
            Ok(project::Status::Scalar(value)) => Ok(Status::Scalar(value)),
            Ok(project::Status::Complete) => Ok(Status::Complete),
            Ok(project::Status::Begin | project::Status::End) => Err(Error::InvalidState),
            Err(error) => Err(error.into()),
        };
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
    use crate::{
        header_address_text::Cursor,
        time::Deadline,
        work::{Charge, Stop},
    };
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
    fn drain(cursor: &mut Budgeted<'_, '_>) -> (String, bool, Result<(), Error>) {
        assert!(std::mem::size_of_val(cursor) <= 448);
        let mut text = String::new();
        for _ in 0..1_000_000 {
            let (before, steps) = cursor.inner.remaining();
            let result = cursor.poll(Tick(1));
            let (after, remaining) = cursor.inner.remaining();
            assert!(before.io_bytes - after.io_bytes <= 160);
            assert!(before.records - after.records <= 16);
            assert!(steps - remaining <= 255);
            assert!(before.output_bytes - after.output_bytes <= 4);
            match result {
                Ok(Status::Yield) => {}
                Ok(Status::Scalar(value)) => text.push(value),
                Ok(Status::Complete) => {
                    assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                    assert_eq!(cursor.inner.remaining(), (after, remaining));
                    return (text, cursor.is_encoding_problem(), Ok(()));
                }
                Err(error) => return (text, cursor.is_encoding_problem(), Err(error)),
            }
        }
        panic!("budgeted address text did not finish");
    }
    #[test]
    fn conversion_preserves_plain_text_diagnostics_visits_and_output_costs() {
        let long = format!("{}@b", "🐈".repeat(4096));
        let folds = format!(" \tbad{}addr\r\n ", "\r\n ".repeat(4096));
        let nested = "(".repeat(33);
        for source in [
            b"".as_slice(),
            b" \t\r\n ",
            b"a@b",
            b" (n) a . b @ EXAMPLE (tail)",
            b"\"a\r\n b\"@[x\n\ty]",
            b"\"\\\0\"@b",
            b"a@b bad",
            b"a@b\xff",
            b"=?utf-8?q?name?=@b",
            b"\xffx\xe2\x82",
            b"\xf0\x80\x80\xaf",
            b"\0\x01\x7f",
            "e\u{301}@例\u{fdd0}".as_bytes(),
            long.as_bytes(),
            folds.as_bytes(),
            nested.as_bytes(),
        ] {
            for mode in [Mode::Parsed, Mode::Fallback] {
                let mut plain_work = work();
                let mut plain = Cursor::new(source, mode);
                let mut text = String::new();
                let result = loop {
                    match plain.poll(Tick(1), &mut plain_work) {
                        Ok(Status::Yield) => {}
                        Ok(Status::Scalar(value)) => text.push(value),
                        Ok(Status::Complete) => break Ok(()),
                        Err(error) => break Err(error),
                    }
                };
                let mut work = work();
                let mut budget = HeaderBudget::new();
                assert_eq!(
                    drain(&mut Budgeted::new(source, mode, &mut work, &mut budget)),
                    (text, plain.is_encoding_problem(), result)
                );
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
            }
        }
        assert_eq!(
            drain(&mut Budgeted::new(
                "e\u{301}@EXAMPLE".as_bytes(),
                Mode::Parsed,
                &mut work(),
                &mut HeaderBudget::new()
            )),
            ("e\u{301}@EXAMPLE".to_owned(), false, Ok(()))
        );
    }
    #[test]
    fn all_partial_byte_and_step_allowances_retire_provisional_text() {
        let mut late = false;
        for (source, mode) in [
            (b"a@b".as_slice(), Mode::Parsed),
            (b"a@b bad", Mode::Parsed),
            (b"\"a\r\n b\"@[x\n\ty]", Mode::Parsed),
            ("\u{fdd0}@b".as_bytes(), Mode::Parsed),
            (b" \tbad\r\n addr ", Mode::Fallback),
            (b"\xffx\xe2\x82", Mode::Fallback),
        ] {
            let mut work = work();
            let mut budget = HeaderBudget::new();
            let expected = drain(&mut Budgeted::new(source, mode, &mut work, &mut budget));
            if expected.2 == Err(Error::Malformed) {
                assert!(expected.0.is_empty());
            }
            let visits = 16 * 1024 * 1024 - budget.source_bytes_remaining();
            let steps = 16_000_000 - budget.steps_remaining();
            for (bytes, steps) in (0..steps)
                .map(|cut| (visits, cut))
                .chain((0..visits).map(|cut| (cut, steps)))
            {
                let mut work = self::work();
                let mut budget = limited(bytes, steps);
                let mut cursor = Budgeted::new(source, mode, &mut work, &mut budget);
                let (text, _, result) = drain(&mut cursor);
                assert_eq!(result, Err(Error::InterpretationLimit));
                assert!(expected.0.starts_with(&text));
                late |= !text.is_empty();
                let after = cursor.inner.remaining();
                assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
                assert_eq!(
                    cursor.check_deadline(Tick(1)),
                    Err(Error::InterpretationLimit)
                );
                assert_eq!(cursor.inner.remaining(), after);
                assert_eq!(work.stopped(), None);
                assert_eq!(
                    Budgeted::new(b"", Mode::Fallback, &mut work, &mut budget).poll(Tick(1)),
                    Err(Error::InterpretationLimit)
                );
            }
            let mut work = self::work();
            let mut budget = limited(visits, steps);
            assert_eq!(
                drain(&mut Budgeted::new(source, mode, &mut work, &mut budget)),
                expected
            );
        }
        assert!(late);
    }
    #[test]
    fn parsed_replay_and_fallback_charge_intermediate_and_scalar_bytes() {
        for (mode, visits) in [(Mode::Parsed, 24), (Mode::Fallback, 8)] {
            let mut work = work();
            let mut budget = HeaderBudget::new();
            assert_eq!(
                drain(&mut Budgeted::new(b"a@b", mode, &mut work, &mut budget)),
                ("a@b".to_owned(), false, Ok(()))
            );
            assert_eq!(100_000_000 - work.remaining().io_bytes, visits);
            assert_eq!(1_000_000 - work.remaining().output_bytes, 6);
            for output_bytes in 0..6 {
                let mut work = Meter::new(
                    Deadline::after(Tick(0), 100).unwrap(),
                    Charge {
                        output_bytes,
                        ..self::work().remaining()
                    },
                );
                let mut budget = HeaderBudget::new();
                let mut cursor = Budgeted::new(b"a@b", mode, &mut work, &mut budget);
                let (text, _, result) = drain(&mut cursor);
                assert!("a@b".starts_with(&text));
                assert_eq!(result, Err(Error::Work(Stop::OutputBytes)));
                let after = cursor.inner.remaining();
                assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::OutputBytes)));
                assert_eq!(cursor.inner.remaining(), after);
                assert!(budget.steps_remaining() < 16_000_000);
                budget
                    .charge_local(&mut self::work(), Tick(1), 0, 0, &mut 0)
                    .unwrap();
            }
        }
    }
    #[test]
    fn private_credit_eof_and_final_admission_are_not_replayable() {
        let mut work = work();
        let mut budget = limited(0, 3);
        for remaining in [2, 1, 0] {
            assert_eq!(
                drain(&mut Budgeted::new(
                    b"",
                    Mode::Fallback,
                    &mut work,
                    &mut budget
                )),
                (String::new(), false, Ok(()))
            );
            assert_eq!(budget.steps_remaining(), remaining);
        }
        assert_eq!(work.remaining().records, 1_999_997);
        assert_eq!(
            Budgeted::new(b"", Mode::Fallback, &mut work, &mut budget).poll(Tick(1)),
            Err(Error::InterpretationLimit)
        );
        let mut budget = HeaderBudget::new();
        let mut cursor = Budgeted::new(b"a@b", Mode::Parsed, &mut work, &mut budget);
        assert_eq!(drain(&mut cursor).2, Ok(()));
        let before = cursor.inner.remaining();
        for _ in 0..100 {
            cursor.check_deadline(Tick(1)).unwrap();
        }
        assert_eq!(cursor.inner.remaining(), before);
        assert_eq!(
            cursor.check_deadline(Tick(100)),
            Err(Error::Work(Stop::Deadline))
        );
        assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
    }
    #[test]
    fn job_and_nesting_errors_remain_distinct() {
        for mode in [Mode::Parsed, Mode::Fallback] {
            for (io_bytes, records, now, stop) in [
                (0, 1000, Tick(1), Stop::IoBytes),
                (1000, 0, Tick(1), Stop::Records),
                (1000, 1000, Tick(100), Stop::Deadline),
            ] {
                let mut work = Meter::new(
                    Deadline::after(Tick(0), 100).unwrap(),
                    Charge {
                        io_bytes,
                        records,
                        output_bytes: 1000,
                        ..Charge::default()
                    },
                );
                let mut budget = HeaderBudget::new();
                let mut cursor = Budgeted::new(b"a@b", mode, &mut work, &mut budget);
                assert_eq!(cursor.poll(now), Err(Error::Work(stop)));
                assert_eq!(cursor.check_deadline(Tick(1)), Err(Error::Work(stop)));
                assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(stop)));
                assert_eq!(work.stopped(), Some(stop));
                budget
                    .charge_local(&mut self::work(), Tick(1), 0, 0, &mut 0)
                    .unwrap();
            }
        }
        let nested = "(".repeat(33);
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let mut cursor = Budgeted::new(nested.as_bytes(), Mode::Parsed, &mut work, &mut budget);
        assert_eq!(drain(&mut cursor).2, Err(Error::NestingLimit));
        assert_eq!(cursor.poll(Tick(1)), Err(Error::NestingLimit));
    }
}
