//! Provisional address/group grammar events under the shared email budget.
use super::{Cursor, Error, Phase, Status};
use crate::{
    admission::work::Meter,
    decode_work::{Error as DecodeError, Parsing},
    nfc::HeaderBudget,
    ports::Tick,
};
/// Retains exclusive budget borrows. All events retire if final validation fails.
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {}
/// cloned::<td_mta::header_addresses::Budgeted<'_, '_>>();
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
        header_addresses::Address,
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
        assert!(std::mem::size_of_val(cursor) <= 800);
        let mut events = Vec::new();
        for _ in 0..1_000_000 {
            let before = cursor.work.remaining();
            let steps = cursor.budget.steps_remaining();
            let result = cursor.poll(Tick(1));
            let after = cursor.work.remaining();
            assert!(before.io_bytes - after.io_bytes <= 161);
            assert!(before.records - after.records <= 13);
            assert!(steps - cursor.budget.steps_remaining() <= 196);
            assert_eq!(before.output_bytes, after.output_bytes);
            match result {
                Err(error) => return (events, Err(error)),
                Ok(Status::Complete) => {
                    assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                    assert_eq!(cursor.work.remaining(), after);
                    return (events, Ok(()));
                }
                Ok(Status::Yield) => {}
                Ok(event) => events.push(event),
            }
        }
        panic!("budgeted address list did not finish");
    }
    #[test]
    fn shared_budget_preserves_plain_events_results_and_visits() {
        let long = format!(
            "{}: {} <@route:é@例.test>, bad;",
            "🐈".repeat(4096),
            "é".repeat(4096)
        );
        let comment = format!(
            "a@b ({}), \"{}\"@[{}]",
            "🐈".repeat(4096),
            "x".repeat(4096),
            "y".repeat(4096)
        );
        let nested = format!("a@b,{}", "(".repeat(33));
        for source in [
            b"".as_slice(),
            b", (c),; ;\t",
            b"a@b",
            b"G:;H:c@d; a@b;",
            b"G:a@b,H:c@d;",
            b"bad, a@b,\"unclosed,tail",
            b"@bad:a@b, valid@b;",
            b"\"Doe, Jo\" <@a,@b: q@[x y]> (tail)",
            b"a@b (Name)",
            b" \tbad\r\n ",
            b"(\xff) a@b",
            b"(a\0b) G:c@d;",
            b"\0\xff",
            b"\"\\\xe2\x82",
            long.as_bytes(),
            comment.as_bytes(),
            nested.as_bytes(),
        ] {
            let mut plain_work = work();
            let mut plain = Cursor::new(source);
            let mut expected = Vec::new();
            let result = loop {
                match plain.poll(Tick(1), &mut plain_work) {
                    Ok(Status::Yield) => {}
                    Ok(Status::Complete) => break Ok(()),
                    Ok(event) => expected.push(event),
                    Err(error) => break Err(error),
                }
            };
            let mut work = work();
            let mut budget = HeaderBudget::new();
            assert_eq!(
                drain(&mut Budgeted::new(source, &mut work, &mut budget)),
                (expected, result)
            );
            assert_eq!(work.remaining().io_bytes, plain_work.remaining().io_bytes);
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
    #[test]
    fn every_partial_budget_retires_groups_and_never_becomes_raw_recovery() {
        let mut closed = false;
        let mut parsed = false;
        let mut raw = false;
        for source in [
            b"a@b".as_slice(),
            b"G:; H: a@b,c@d;",
            b"bad, a@b,\"unclosed",
            b"\"Jo\" <@a,@b:q@[x y]> (tail)",
            b"a@b (Name)",
            b"@bad:a@b, valid@b;",
            b"(\xff) a@b",
            b"\"\\\xe2\x82",
            b" (a\r\nb) bad ",
        ] {
            let mut work = work();
            let mut budget = HeaderBudget::new();
            let expected = drain(&mut Budgeted::new(source, &mut work, &mut budget));
            let visits = 16 * 1024 * 1024 - budget.source_bytes_remaining();
            let steps = 16_000_000 - budget.steps_remaining();
            for (bytes, steps) in (0..steps)
                .map(|cut| (visits, cut))
                .chain((0..visits).map(|cut| (cut, steps)))
            {
                let mut work = self::work();
                let mut budget = limited(bytes, steps);
                let mut cursor = Budgeted::new(source, &mut work, &mut budget);
                let (events, result) = drain(&mut cursor);
                assert_eq!(result, Err(Error::InterpretationLimit));
                assert!(expected.0.starts_with(&events));
                closed |= events.contains(&Status::EndGroup);
                parsed |= events
                    .iter()
                    .any(|s| matches!(s, Status::Mailbox(Address::Parsed(_))));
                raw |= events
                    .iter()
                    .any(|s| matches!(s, Status::Mailbox(Address::Raw(_))));
                let before = cursor.work.remaining();
                assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
                assert_eq!(
                    cursor.check_deadline(Tick(1)),
                    Err(Error::InterpretationLimit)
                );
                assert_eq!(cursor.work.remaining(), before);
                assert_eq!(work.stopped(), None);
                assert_eq!(
                    Budgeted::new(b"", &mut work, &mut budget).poll(Tick(1)),
                    Err(Error::InterpretationLimit)
                );
            }
            let mut work = self::work();
            let mut budget = limited(visits, steps);
            assert_eq!(
                drain(&mut Budgeted::new(source, &mut work, &mut budget)),
                expected
            );
        }
        assert!(
            closed && parsed && raw,
            "fixtures must cover late refusal after every event kind"
        );
    }
    #[test]
    fn empty_fields_have_exact_eof_costs_and_private_credit() {
        let mut work = work();
        let mut budget = limited(0, 21);
        for remaining in [14, 7, 0] {
            assert_eq!(
                drain(&mut Budgeted::new(b"", &mut work, &mut budget)),
                (Vec::new(), Ok(()))
            );
            assert_eq!(budget.steps_remaining(), remaining);
            assert_eq!(work.remaining().io_bytes, 100_000_000);
        }
        assert_eq!(work.remaining().records, 1_999_997);
        assert_eq!(
            Budgeted::new(b"", &mut work, &mut budget).poll(Tick(1)),
            Err(Error::InterpretationLimit)
        );
    }
    #[test]
    fn refusal_keeps_item_selection_unpublished_and_preserves_prior_charges() {
        for (bytes, steps) in [(0, 100), (100, 0), (100, 1)] {
            let mut work = work();
            let mut budget = limited(bytes, steps);
            let mut cursor = Budgeted::new(b"G:a@b;", &mut work, &mut budget);
            assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
            assert!(matches!(cursor.cursor.phase, Phase::Item));
            assert!(cursor.cursor.item.is_none());
            assert_eq!(cursor.work.remaining().io_bytes, 100_000_000);
            assert_eq!(cursor.budget.steps_remaining(), steps.saturating_sub(1));
        }
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
            let mut cursor = Budgeted::new(b"a@b", &mut work, &mut budget);
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(stop)));
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
    }
    #[test]
    fn nesting_and_final_deadline_retire_all_provisional_results() {
        let nested = format!("a@b,{}", "(".repeat(33));
        let mut work = work();
        let mut budget = HeaderBudget::new();
        let mut cursor = Budgeted::new(nested.as_bytes(), &mut work, &mut budget);
        let (events, result) = drain(&mut cursor);
        assert!(events.iter().any(|s| matches!(s, Status::Mailbox(_))));
        assert_eq!(result, Err(Error::NestingLimit));
        let after = cursor.work.remaining();
        assert_eq!(cursor.poll(Tick(1)), Err(Error::NestingLimit));
        assert_eq!(cursor.work.remaining(), after);
        let mut cursor = Budgeted::new(b"a@b", &mut work, &mut budget);
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
}
