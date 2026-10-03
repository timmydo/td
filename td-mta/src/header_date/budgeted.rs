//! Date parsing under the original job and per-email interpretation budgets.
use super::{Cursor, Error, Status};
use crate::{
    admission::work::{Charge, Meter},
    decode_work::{Error as DecodeError, Parsing},
    nfc::HeaderBudget,
    ports::Tick,
};
/// Retains exclusive budget borrows and non-replayable prepaid work credit.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {}
/// cloned::<td_mta::header_date::Budgeted<'_, '_>>();
/// ```
pub struct Budgeted<'a, 'w> {
    cursor: Cursor<'a>,
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    credit: u8,
    failure: Option<Error>,
}
impl<'a, 'w> Budgeted<'a, 'w> {
    pub fn new(source: &'a [u8], work: &'w mut Meter, budget: &'w mut HeaderBudget) -> Self {
        Self {
            cursor: Cursor::new(source),
            work,
            budget,
            credit: 0,
            failure: None,
        }
    }
    pub(crate) fn finish(self) -> Result<(&'w mut Meter, &'w mut HeaderBudget), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.cursor.complete.is_none() {
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
    /// Final admission stays live even after a cached Complete result.
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
        if let Some(value) = self.cursor.complete {
            return Ok(Status::Complete(value));
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
    use crate::{admission::work::Stop, ports::Deadline};
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
    fn drain(cursor: &mut Budgeted<'_, '_>) -> Result<Option<super::super::Date>, Error> {
        assert!(std::mem::size_of_val(cursor) <= 224);
        for _ in 0..100_000 {
            let before = cursor.work.remaining();
            let steps = cursor.budget.steps_remaining();
            let status = cursor.poll(Tick(1));
            let after = cursor.work.remaining();
            assert!(before.io_bytes - after.io_bytes <= 161);
            assert!(before.records - after.records <= 13);
            assert!(steps - cursor.budget.steps_remaining() <= 194);
            assert_eq!(before.output_bytes, after.output_bytes);
            if let Status::Complete(value) = status? {
                assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete(value)));
                assert_eq!(cursor.work.remaining(), after);
                return Ok(value);
            }
        }
        panic!("budgeted date did not finish");
    }
    #[test]
    fn budgeted_dates_preserve_plain_results_and_visits() {
        let long = format!("({})21 Nov 1997 09:55:06 CST", "🐈".repeat(4096));
        let nested = "(".repeat(33);
        for source in [
            b"Fri, 21 Nov 1997 09:55:06 -0600".as_slice(),
            b"1 Jan 2000 00:00 -0000",
            b"31 Dec 2016 23:59:60 +0000",
            b"(a(b)c)1 Jan 2000 00:00 GMT (tail)",
            b"1 Jan 200000:00 +0000",
            b"31 Feb 2000 00:00 +0000",
            b"1 Jan 2000 00:00 +0000 (bad",
            b"(\xff)1 Jan 2000 00:00 +0000",
            long.as_bytes(),
            nested.as_bytes(),
        ] {
            let mut plain_work = work();
            let mut plain = Cursor::new(source);
            let expected = loop {
                match plain.poll(Tick(1), &mut plain_work) {
                    Ok(Status::Yield) => {}
                    Ok(Status::Complete(value)) => break Ok(value),
                    Err(error) => break Err(error),
                }
            };
            let mut work = work();
            let mut budget = HeaderBudget::new();
            let initial_steps = budget.steps_remaining();
            let mut cursor = Budgeted::new(source, &mut work, &mut budget);
            assert_eq!(drain(&mut cursor), expected);
            assert_eq!(work.remaining().io_bytes, plain_work.remaining().io_bytes);
            assert_eq!(
                2_000_000 - work.remaining().records,
                (initial_steps - budget.steps_remaining()).div_ceil(16)
            );
        }
    }
    #[test]
    fn eof_and_cfws_revisits_have_exact_charges() {
        for (source, visits, steps) in [
            (b"".as_slice(), 0, 4),
            (b" ", 2, 7),
            (b"()", 3, 9),
            (b"\r\n ", 5, 11),
        ] {
            let mut work = work();
            let mut budget = HeaderBudget::new();
            let bytes_before = budget.source_bytes_remaining();
            let steps_before = budget.steps_remaining();
            let mut cursor = Budgeted::new(source, &mut work, &mut budget);
            assert_eq!(drain(&mut cursor), Ok(None));
            assert_eq!(100_000_000 - work.remaining().io_bytes, visits);
            assert_eq!(bytes_before - budget.source_bytes_remaining(), visits);
            assert_eq!(steps_before - budget.steps_remaining(), steps);
            assert_eq!(2_000_000 - work.remaining().records, 1);
        }
    }
    #[test]
    fn aggregate_refusal_precedes_source_access_and_cannot_become_null() {
        for (bytes, steps) in [(0, 100), (100, 0), (100, 1)] {
            let mut work = work();
            let mut budget = limited(bytes, steps);
            let mut cursor = Budgeted::new(b"1 Jan 2000 00:00 +0000", &mut work, &mut budget);
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
            let mut next = Budgeted::new(b"", &mut work, &mut budget);
            assert_eq!(next.poll(Tick(1)), Err(Error::InterpretationLimit));
        }
    }
    #[test]
    fn credit_does_not_escape_between_fields_and_final_deadline_stays_live() {
        let mut work = work();
        let mut budget = limited(100, 7);
        assert_eq!(
            drain(&mut Budgeted::new(b"", &mut work, &mut budget)),
            Ok(None)
        );
        assert_eq!(budget.steps_remaining(), 3);
        let mut cursor = Budgeted::new(b"", &mut work, &mut budget);
        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
        assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
        assert_eq!(work.remaining().records, 1_999_998);
        assert_eq!(work.stopped(), None);
        let mut budget = HeaderBudget::new();
        let mut cursor = Budgeted::new(b"", &mut work, &mut budget);
        assert_eq!(drain(&mut cursor), Ok(None));
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
    fn job_exhaustion_remains_distinct_from_the_email_limit() {
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
            let mut cursor = Budgeted::new(b"1", &mut work, &mut budget);
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(stop)));
            assert_eq!(cursor.cursor.position, 0);
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(stop)));
            assert_eq!(work.stopped(), Some(stop));
            assert_eq!(budget.source_bytes_remaining(), 16 * 1024 * 1024);
            // The first CFWS record succeeds before an I/O-byte refusal.
            assert_eq!(
                budget.steps_remaining(),
                16_000_000 - u64::from(stop == Stop::IoBytes)
            );
            budget
                .charge(&mut self::work(), Tick(1), 0, 0, &mut 0)
                .unwrap();
        }
    }
    #[test]
    fn every_partial_budget_refuses_before_a_malformed_or_valid_outcome() {
        for source in [
            b"(\xe2\x82".as_slice(),
            b"(\xff)1 Jan 2000 00:00 +0000",
            b"1 Jan 2000 00:00 +0000 (bad",
            b"\r\n x",
            b"(a\r\n b)1 Jan 2000 00:00 +0000 (tail)",
            b"(\\\xe2\x82)1 Jan 2000 00:00 +0000",
        ] {
            let mut work = work();
            let mut budget = HeaderBudget::new();
            let expected = drain(&mut Budgeted::new(source, &mut work, &mut budget));
            let steps = 16_000_000 - budget.steps_remaining();
            let visits = 16 * 1024 * 1024 - budget.source_bytes_remaining();
            for (bytes, steps) in (0..steps)
                .map(|cut| (visits, cut))
                .chain((0..visits).map(|cut| (cut, steps)))
            {
                let mut work = self::work();
                let mut budget = limited(bytes, steps);
                let mut cursor = Budgeted::new(source, &mut work, &mut budget);
                assert_eq!(drain(&mut cursor), Err(Error::InterpretationLimit));
                assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
                assert_eq!(work.stopped(), None);
            }
            let mut work = self::work();
            let mut budget = limited(visits, steps);
            assert_eq!(
                drain(&mut Budgeted::new(source, &mut work, &mut budget)),
                expected
            );
        }
    }
}
