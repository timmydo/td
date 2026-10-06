//! Validated URL-header bytes under the original job and email budgets.
use super::{Cursor, Error, Mode, Status};
use crate::{
    decode_work::{Conversion, Error as DecodeError},
    nfc::HeaderBudget,
    time::Tick,
    work::{Charge, Meter},
};
/// Retains exclusive budget borrows; every event remains provisional.
/// The caller authorizes ListPost only for the List-Post field.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {}
/// cloned::<td_mime::header_urls::Budgeted<'_, '_>>();
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
    #[cfg(test)]
    pub(crate) fn remaining(&self) -> (Charge, u64) {
        (self.work.remaining(), self.budget.steps_remaining())
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
    pub(crate) fn finish_malformed(self) -> Result<(&'w mut Meter, &'w mut HeaderBudget), Error> {
        if self.failure != Some(Error::Malformed) || self.cursor.replay {
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
        if self.cursor.complete {
            return Ok(Status::Complete);
        }
        self.check_deadline(now)?;
        let result = self.cursor.poll_with_work(
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
    use crate::{
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
    fn drain(cursor: &mut Budgeted<'_, '_>) -> (Vec<Status>, Result<(), Error>) {
        assert!(std::mem::size_of_val(cursor) <= 288);
        let mut events = Vec::new();
        for _ in 0..100_000 {
            let before = cursor.work.remaining();
            let steps = cursor.budget.steps_remaining();
            let status = cursor.poll(Tick(1));
            let after = cursor.work.remaining();
            assert!(before.io_bytes - after.io_bytes <= 160);
            assert!(before.records - after.records <= 13);
            assert!(steps - cursor.budget.steps_remaining() <= 193);
            assert!(before.output_bytes - after.output_bytes <= 1);
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
        panic!("budgeted URLs did not finish");
    }
    #[test]
    fn urls_preserve_plain_events_visits_and_output() {
        let long = format!(
            "({})<https://example.test/{}>",
            "🐈".repeat(4096),
            "path/".repeat(4096)
        );
        let nested = format!("<x:a>{}", "(".repeat(33));
        for source in [
            b"".as_slice(),
            b"<x:>",
            b"NO",
            b"no",
            b"<mailto:a@b>, <https://x/%20?a=b#c>",
            b"<x://[::1]>",
            b"<x://[v1.a:b]>",
            b"<x://[bad]>",
            b"<x:a>,<bad>",
            b"<x:a> (bad",
            b"<x: a\r\n b>",
            b"<x:%2>",
            b"<x:\xff>",
            long.as_bytes(),
            nested.as_bytes(),
        ] {
            for mode in [Mode::URLs, Mode::ListPost] {
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
    fn exact_two_pass_and_ipv6_costs_include_eof_and_emitted_bytes() {
        for (source, mode, visits, steps, output) in [
            (b"<x:>".as_slice(), Mode::URLs, 10, 34, 2),
            (b"NO", Mode::ListPost, 6, 24, 0),
            (b"<x://[::1]>", Mode::URLs, 24, 197, 9),
        ] {
            let mut work = work();
            let mut budget = HeaderBudget::new();
            assert_eq!(
                drain(&mut Budgeted::new(source, mode, &mut work, &mut budget)).1,
                Ok(())
            );
            assert_eq!(100_000_000 - work.remaining().io_bytes, visits);
            assert_eq!(16_000_000 - budget.steps_remaining(), steps);
            assert_eq!(1_000_000 - work.remaining().output_bytes, output);
        }
    }
    #[test]
    fn all_partial_budgets_refuse_in_validation_and_replay() {
        let mut late = false;
        for source in [
            b"<x:a>".as_slice(),
            b"NO",
            b"<x://[::1]>",
            b"<x://[bad]>",
            b"<x:a> (bad",
            b"<x:a\r\n b>",
            b"(\xe2\x82",
            b"<x:%2>",
        ] {
            let mut work = work();
            let mut budget = HeaderBudget::new();
            let expected = drain(&mut Budgeted::new(
                source,
                Mode::ListPost,
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
                let mut cursor = Budgeted::new(source, Mode::ListPost, &mut work, &mut budget);
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
                    Mode::ListPost,
                    &mut work,
                    &mut budget
                )),
                expected
            );
        }
        assert!(late);
    }
    #[test]
    fn preaccess_refusal_field_credit_and_final_admission_remain_owned() {
        for (bytes, steps) in [(0, 100), (100, 0), (100, 1), (100, 2)] {
            let mut work = work();
            let mut budget = limited(bytes, steps);
            let mut cursor = Budgeted::new(b"<x:a>", Mode::URLs, &mut work, &mut budget);
            assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
            assert_eq!(cursor.cursor.position, 0);
            assert_eq!(cursor.work.remaining().io_bytes, 100_000_000);
            assert_eq!(
                cursor.check_deadline(Tick(1)),
                Err(Error::InterpretationLimit)
            );
            assert_eq!(
                Budgeted::new(b"NO", Mode::ListPost, &mut work, &mut budget).poll(Tick(1)),
                Err(Error::InterpretationLimit)
            );
        }
        let mut work = work();
        let mut budget = limited(12, 48);
        for steps in [24, 0] {
            assert_eq!(
                drain(&mut Budgeted::new(
                    b"NO",
                    Mode::ListPost,
                    &mut work,
                    &mut budget
                ))
                .1,
                Ok(())
            );
            assert_eq!(budget.steps_remaining(), steps);
        }
        assert_eq!(work.remaining().records, 1_999_996);
        let mut budget = HeaderBudget::new();
        let mut cursor = Budgeted::new(b"<x:a>", Mode::URLs, &mut work, &mut budget);
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
    fn job_stops_do_not_become_malformed_or_exhaust_the_email() {
        for (io_bytes, records, output_bytes, stop) in [
            (0, 1000, 1000, Stop::IoBytes),
            (1000, 0, 1000, Stop::Records),
            (1000, 1000, 0, Stop::OutputBytes),
            (1000, 1000, 1, Stop::OutputBytes),
        ] {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes,
                    records,
                    output_bytes,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            let mut cursor = Budgeted::new(b"<x:a>", Mode::URLs, &mut work, &mut budget);
            assert_eq!(drain(&mut cursor).1, Err(Error::Work(stop)));
            let after = cursor.work.remaining();
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(stop)));
            assert_eq!(cursor.work.remaining(), after);
            assert_eq!(work.stopped(), Some(stop));
            budget
                .charge_local(&mut self::work(), Tick(1), 0, 0, &mut 0)
                .unwrap();
        }
    }
}
