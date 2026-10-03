//! Provisional MessageIds grammar events under the shared email budget.
use super::{Cursor, Error, Mode, Phase, Status};
use crate::{
    admission::work::Meter,
    decode_work::{Error as DecodeError, Parsing},
    nfc::HeaderBudget,
    ports::Tick,
};
/// Retains exclusive budget borrows. All events retire if final validation fails.
/// The caller authorizes ObsoletePhrases only for References or In-Reply-To.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {}
/// cloned::<td_mta::header_message_ids::Budgeted<'_, '_>>();
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
        Self {
            cursor: Cursor::new(source, mode),
            work,
            budget,
            credit: 0,
            failure: None,
        }
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self
            .budget
            .charge(self.work, now, 0, 0, &mut self.credit)
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
        let result = self.cursor.poll_with_work(
            now,
            &mut Parsing::new(self.work, self.budget, &mut self.credit),
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
    use crate::{
        admission::work::{Charge, Stop},
        ports::Deadline,
    };
    fn work() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 2_000_000,
                output_bytes: 100,
                ..Charge::default()
            },
        )
    }
    fn limited(bytes: u64, steps: u64) -> HeaderBudget {
        let mut budget = HeaderBudget::new();
        budget
            .charge(
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
        assert!(std::mem::size_of_val(cursor) <= 288);
        let mut events = Vec::new();
        for _ in 0..100_000 {
            let before = cursor.work.remaining();
            let steps = cursor.budget.steps_remaining();
            let status = cursor.poll(Tick(1));
            let after = cursor.work.remaining();
            assert!(before.io_bytes - after.io_bytes <= 160);
            assert!(before.records - after.records <= 12);
            assert!(steps - cursor.budget.steps_remaining() <= 192);
            assert_eq!(before.output_bytes, after.output_bytes);
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
        panic!("budgeted MessageIds did not finish");
    }
    #[test]
    fn budgeted_lists_preserve_plain_events_results_and_visits() {
        let long = format!("({0})<{0}@b><\"{0}\"@[{0}]>", "🐈".repeat(4096));
        let nested = "(".repeat(33);
        for source in [
            b"".as_slice(),
            b"<a@b>",
            b"old words <a@b> more.words",
            b"<a@b> (bad",
            b"<a@b><c@d>",
            b"<\"a\r\n b\"@[c\n\td]>",
            b"<\"\\\0\"@[\\\r]>",
            b"<\xe2\x82",
            b"<a@[\xff]>",
            "<é@例.テスト>".as_bytes(),
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
                assert_eq!(
                    drain(&mut Budgeted::new(source, mode, &mut work, &mut budget)),
                    (events, result)
                );
                assert_eq!(work.remaining().io_bytes, plain_work.remaining().io_bytes);
                let visits = 100_000_000 - work.remaining().io_bytes;
                assert_eq!(16 * 1024 * 1024 - budget.source_bytes_remaining(), visits);
                assert_eq!(
                    2_000_000 - work.remaining().records,
                    (16_000_000 - budget.steps_remaining()).div_ceil(16)
                );
            }
        }
    }
    #[test]
    fn exact_eof_and_revisit_costs() {
        for (source, visits, steps, result) in [
            (b"".as_slice(), 0, 3, Err(Error::Malformed)),
            (b"<a@b>", 14, 31, Ok(())),
        ] {
            let mut work = work();
            let mut budget = HeaderBudget::new();
            assert_eq!(
                drain(&mut Budgeted::new(
                    source,
                    Mode::Strict,
                    &mut work,
                    &mut budget
                ))
                .1,
                result
            );
            assert_eq!(100_000_000 - work.remaining().io_bytes, visits);
            assert_eq!(16 * 1024 * 1024 - budget.source_bytes_remaining(), visits);
            assert_eq!(16_000_000 - budget.steps_remaining(), steps);
        }
    }
    #[test]
    fn aggregate_limits_precede_access_and_latch_across_fields() {
        for (bytes, steps) in [(0, 100), (100, 0), (100, 1)] {
            let mut work = work();
            let mut budget = limited(bytes, steps);
            let mut cursor = Budgeted::new(b"<a@b>", Mode::Strict, &mut work, &mut budget);
            assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
            assert_eq!(cursor.cursor.position, 0);
            assert_eq!(cursor.cursor.cfws.as_ref().unwrap().position(), 0);
            let after = cursor.work.remaining();
            assert_eq!(after.io_bytes, 100_000_000);
            assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
            assert_eq!(
                cursor.check_deadline(Tick(1)),
                Err(Error::InterpretationLimit)
            );
            assert_eq!(cursor.work.remaining(), after);
            assert_eq!(work.stopped(), None);
            assert_eq!(
                Budgeted::new(b"", Mode::ObsoletePhrases, &mut work, &mut budget).poll(Tick(1)),
                Err(Error::InterpretationLimit)
            );
        }
    }
    #[test]
    fn every_partial_budget_retires_all_provisional_events() {
        let mut late = false;
        for source in [
            b"<a@b>".as_slice(),
            b"<a@b> (bad",
            b"<\xe2\x82",
            b"<\"\\\xe2\x82",
            b"<a@[b\r\n x]>",
            b"<a@b> <c@[\xff]>",
            b"old <a@b> tail",
            b"(x\r\ny)",
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
        assert!(
            late,
            "the fixtures must exercise refusal after an End event"
        );
    }
    #[test]
    fn credit_is_private_to_a_field_and_final_admission_is_live() {
        let mut work = work();
        let mut budget = limited(100, 6);
        for remaining in [3, 0] {
            assert_eq!(
                drain(&mut Budgeted::new(
                    b"",
                    Mode::ObsoletePhrases,
                    &mut work,
                    &mut budget
                )),
                (Vec::new(), Ok(()))
            );
            assert_eq!(budget.steps_remaining(), remaining);
        }
        assert_eq!(work.remaining().records, 1_999_998);
        assert_eq!(
            Budgeted::new(b"", Mode::ObsoletePhrases, &mut work, &mut budget).poll(Tick(1)),
            Err(Error::InterpretationLimit)
        );
        let mut budget = HeaderBudget::new();
        let mut cursor = Budgeted::new(b"<a@b>", Mode::Strict, &mut work, &mut budget);
        assert_eq!(drain(&mut cursor).1, Ok(()));
        let before = cursor.work.remaining();
        for _ in 0..100 {
            cursor.check_deadline(Tick(1)).unwrap();
        }
        assert_eq!(cursor.work.remaining(), before);
        assert_eq!(
            cursor.check_deadline(Tick(100)),
            Err(Error::Work(Stop::Deadline))
        );
        assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
    }
    #[test]
    fn job_limits_are_distinct_and_preserve_prior_email_costs() {
        for (io_bytes, records, stop) in [(0, 100, Stop::IoBytes), (100, 0, Stop::Records)] {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes,
                    records,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            let mut cursor = Budgeted::new(b"<a@b>", Mode::Strict, &mut work, &mut budget);
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(stop)));
            assert_eq!(cursor.cursor.position, 0);
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(stop)));
            assert_eq!(work.stopped(), Some(stop));
            assert_eq!(budget.source_bytes_remaining(), 16 * 1024 * 1024);
            assert_eq!(
                budget.steps_remaining(),
                16_000_000 - u64::from(stop == Stop::IoBytes)
            );
            budget
                .charge(&mut self::work(), Tick(1), 0, 0, &mut 0)
                .unwrap();
        }
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let nested = "(".repeat(33);
        let mut cursor = Budgeted::new(nested.as_bytes(), Mode::Strict, &mut work, &mut budget);
        assert_eq!(drain(&mut cursor).1, Err(Error::NestingLimit));
        let before = cursor.work.remaining();
        assert_eq!(cursor.poll(Tick(1)), Err(Error::NestingLimit));
        assert_eq!(cursor.work.remaining(), before);
    }
}
