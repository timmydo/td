//! Header scanning and MIME syntax share the email interpretation budget.
use crate::{
    decode_work::{self, Error},
    nfc::HeaderBudget,
    time::Tick,
    work::{Charge as JobCharge, Meter},
};
#[derive(Clone, Copy, Default)]
pub(crate) struct Charge {
    pub visits: u64,
    pub steps: u64,
    pub records: u64,
}
pub(crate) trait Work {
    fn charge(&mut self, now: Tick, charge: Charge) -> Result<(), Error>;
    fn scan_limit(&self) -> usize {
        crate::headers::STEP_TRANSITIONS
    }
}
impl Work for Meter {
    fn charge(&mut self, now: Tick, charge: Charge) -> Result<(), Error> {
        Meter::charge(
            self,
            now,
            JobCharge {
                io_bytes: charge.visits,
                records: charge.records,
                ..JobCharge::default()
            },
        )
        .map_err(Error::Work)
    }
}
pub(crate) struct Aggregate<'w> {
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    credit: &'w mut u8,
}
impl<'w> Aggregate<'w> {
    pub(crate) fn new(
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
        credit: &'w mut u8,
    ) -> Self {
        Self {
            work,
            budget,
            credit,
        }
    }
}
impl Work for Aggregate<'_> {
    fn scan_limit(&self) -> usize {
        // Reserve one step for emission after the final lookahead.
        crate::headers::STEP_TRANSITIONS - 1
    }
    fn charge(&mut self, now: Tick, charge: Charge) -> Result<(), Error> {
        if charge.records > charge.steps {
            return Err(Error::InvalidState);
        }
        self.budget
            .charge_local(self.work, now, charge.visits, charge.steps, self.credit)
            .map_err(Error::from)
    }
}

impl decode_work::Work for Aggregate<'_> {
    fn charge(&mut self, now: Tick, charge: JobCharge) -> Result<(), Error> {
        decode_work::Work::charge(
            &mut decode_work::Parsing::new(self.work, self.budget, self.credit),
            now,
            charge,
        )
    }
}
