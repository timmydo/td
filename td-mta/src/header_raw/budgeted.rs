//! Own Raw state and non-replayable credit while borrowing the live budgets.
use super::{Cursor, Error, Status};
use crate::{
    admission::work::{Charge, Meter},
    decode_work::{Error as DecodeError, Parsing},
    nfc::HeaderBudget,
    ports::Tick,
};

/// The owner retains this borrow across turns and shares its email budget
/// with subsequent projections. This wrapper cannot copy prepaid work credit.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {}
/// cloned::<td_mta::header_raw::Budgeted<'_, '_>>();
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
    pub const fn position(&self) -> usize {
        self.cursor.position()
    }
    pub const fn is_encoding_problem(&self) -> bool {
        self.cursor.is_encoding_problem()
    }
    pub(crate) fn finish(self) -> Result<(&'w mut Meter, &'w mut HeaderBudget), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !self.cursor.complete {
            return Err(Error::InvalidState);
        }
        Ok((self.work, self.budget))
    }
    pub(crate) fn charge_output(&mut self, now: Tick, output_bytes: u64) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self
            .budget
            .charge(self.work, now, 0, 0, &mut self.credit)
            .map_err(DecodeError::from)
            .map_err(Error::from)
            .and_then(|()| {
                self.work
                    .charge(
                        now,
                        Charge {
                            output_bytes,
                            ..Charge::default()
                        },
                    )
                    .map_err(Error::Work)
            });
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.cursor.complete {
            return Ok(Status::Complete);
        }
        let result = self
            .budget
            .charge(self.work, now, 0, 1, &mut self.credit)
            .map_err(DecodeError::from)
            .map_err(Error::from)
            .and_then(|()| {
                self.cursor.poll_with_work(
                    now,
                    &mut Parsing::new(self.work, self.budget, &mut self.credit),
                )
            });
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
    #[test]
    fn budget_handoff_requires_successful_complete_state() {
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let cursor = Budgeted::new(b"a", &mut work, &mut budget);
        assert!(matches!(cursor.finish(), Err(Error::InvalidState)));
        let mut cursor = Budgeted::new(b"a", &mut work, &mut budget);
        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Scalar('a')));
        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Complete));
        let before = (cursor.work.remaining(), cursor.budget.steps_remaining());
        let (work, budget) = cursor.finish().unwrap();
        assert_eq!((work.remaining(), budget.steps_remaining()), before);
        let mut cursor = Budgeted::new(b"", work, budget);
        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Complete));
        assert_eq!(
            cursor.charge_output(Tick(100), 0),
            Err(Error::Work(Stop::Deadline))
        );
        assert!(matches!(cursor.finish(), Err(Error::Work(Stop::Deadline))));
    }
    #[test]
    fn budgeted_raw_preserves_identity_and_exact_scalar_costs() {
        for input in [
            b"".as_slice(),
            b"e\xcc\x81\r\n\t=?utf-8?Q?a?=",
            b"\xe1\0\x80",
            b"\xef\xb7\x90",
            b"a\x01",
        ] {
            let expected = super::super::tests::project(input);
            let mut work = work();
            let mut budget = HeaderBudget::new();
            let mut cursor = Budgeted::new(input, &mut work, &mut budget);
            assert!(std::mem::size_of_val(&cursor) <= 96);
            let mut output = String::new();
            let mut complete = false;
            for _ in 0..1000 {
                let bytes = cursor.budget.source_bytes_remaining();
                let steps = cursor.budget.steps_remaining();
                let records = cursor.work.remaining().records;
                let status = cursor.poll(Tick(1)).unwrap();
                assert!(bytes - cursor.budget.source_bytes_remaining() <= 4);
                assert!(steps - cursor.budget.steps_remaining() <= 6);
                assert!(records - cursor.work.remaining().records <= 1);
                match status {
                    Status::Scalar(c) => output.push(c),
                    Status::Yield => {}
                    Status::Complete => {
                        complete = true;
                        break;
                    }
                }
            }
            assert!(complete);
            assert_eq!((output, cursor.is_encoding_problem()), expected);
            assert_eq!(cursor.position(), input.len());
            let before = (cursor.work.remaining(), cursor.budget.steps_remaining());
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            assert_eq!(
                (cursor.work.remaining(), cursor.budget.steps_remaining()),
                before
            );
        }
        let input = [b'a'; 1000];
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let mut cursor = Budgeted::new(&input, &mut work, &mut budget);
        for _ in 0..1000 {
            assert_eq!(cursor.poll(Tick(1)), Ok(Status::Scalar('a')));
        }
        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Complete));
        assert_eq!(budget.source_bytes_remaining(), 16 * 1024 * 1024 - 1000);
        assert_eq!(budget.steps_remaining(), 16_000_000 - 3002);
        assert_eq!(work.remaining().records, 2_000_000 - 3002_u64.div_ceil(16));
    }
    #[test]
    fn four_byte_scalars_and_malformed_lookahead_pin_exact_work() {
        for (input, visits, steps, peak_steps) in [
            (b"\xf0\x90\x80\x80".as_slice(), 4, 8, 6),
            (b"\xf0\x90\x80", 3, 8, 6),
            (b"\xf0\x90\x80A", 5, 11, 6),
            (b"\xe1\0\x80", 4, 12, 4),
        ] {
            let mut work = work();
            let mut budget = HeaderBudget::new();
            let before = work.remaining();
            let initial = (budget.source_bytes_remaining(), budget.steps_remaining());
            let mut cursor = Budgeted::new(input, &mut work, &mut budget);
            let mut peak = 0;
            let mut peak_visits = 0;
            let mut output = String::new();
            let mut complete = false;
            for _ in 0..20 {
                let bytes = cursor.budget.source_bytes_remaining();
                let steps = cursor.budget.steps_remaining();
                let status = cursor.poll(Tick(1)).unwrap();
                peak = peak.max(steps - cursor.budget.steps_remaining());
                peak_visits = peak_visits.max(bytes - cursor.budget.source_bytes_remaining());
                match status {
                    Status::Scalar(c) => output.push(c),
                    Status::Yield => {}
                    Status::Complete => {
                        complete = true;
                        break;
                    }
                }
            }
            assert!(complete);
            assert_eq!(
                (output, cursor.is_encoding_problem()),
                super::super::tests::project(input)
            );
            assert_eq!(peak, peak_steps);
            assert_eq!(
                peak_visits,
                input
                    .len()
                    .min(if visits == 4 && steps == 12 { 2 } else { 4 }) as u64
            );
            assert_eq!(initial.0 - budget.source_bytes_remaining(), visits);
            assert_eq!(initial.1 - budget.steps_remaining(), steps);
            assert_eq!(before.io_bytes - work.remaining().io_bytes, visits);
            assert_eq!(
                before.records - work.remaining().records,
                steps.div_ceil(16)
            );
        }
    }
    #[test]
    fn byte_and_step_limits_latch_through_fresh_owners() {
        for (bytes, steps) in [(0, 100), (100, 0), (100, 1), (100, 2)] {
            let mut work = work();
            let mut budget = limited(bytes, steps);
            let mut cursor = Budgeted::new(b"a", &mut work, &mut budget);
            assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
            let before = (cursor.work.remaining(), cursor.budget.steps_remaining());
            assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
            assert_eq!(
                (cursor.work.remaining(), cursor.budget.steps_remaining()),
                before
            );
            let mut next = Budgeted::new(b"b", &mut work, &mut budget);
            assert_eq!(next.poll(Tick(1)), Err(Error::InterpretationLimit));
        }
        for (io_bytes, records, now, expected) in [
            (0, 100, Tick(1), Stop::IoBytes),
            (100, 0, Tick(1), Stop::Records),
            (100, 100, Tick(100), Stop::Deadline),
        ] {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes,
                    records,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            let mut cursor = Budgeted::new(b"a", &mut work, &mut budget);
            assert_eq!(cursor.poll(now), Err(Error::Work(expected)));
            let before = cursor.work.remaining();
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(expected)));
            assert_eq!(cursor.work.remaining(), before);
        }
    }
}
