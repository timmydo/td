//! Private charging seam for decoding under job and aggregate header budgets.
use crate::{
    admission::work::{Charge, Meter, Stop},
    nfc::{self, HeaderBudget},
    ports::Tick,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Work(Stop),
    InterpretationLimit,
    InvalidState,
}
pub(crate) trait Work {
    fn charge(&mut self, now: Tick, charge: Charge) -> Result<(), Error>;
}
impl Work for Meter {
    fn charge(&mut self, now: Tick, charge: Charge) -> Result<(), Error> {
        Meter::charge(self, now, charge).map_err(Error::Work)
    }
}

impl From<nfc::Error> for Error {
    fn from(error: nfc::Error) -> Self {
        match error {
            nfc::Error::Work(stop) => Self::Work(stop),
            nfc::Error::InterpretationLimit => Self::InterpretationLimit,
            nfc::Error::InvalidState | nfc::Error::InvalidTable => Self::InvalidState,
        }
    }
}
/// Private visit/record admission; the enclosing owner retains credit and failures.
pub(crate) struct Parsing<'w> {
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    credit: &'w mut u8,
}
impl<'w> Parsing<'w> {
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
impl Work for Parsing<'_> {
    fn charge(&mut self, now: Tick, charge: Charge) -> Result<(), Error> {
        if charge.output_bytes != 0 || charge.unlinks != 0 {
            return Err(Error::InvalidState);
        }
        let steps = charge
            .io_bytes
            .checked_add(charge.records)
            .ok_or(Error::InvalidState)?
            .max(1);
        self.budget
            .charge(self.work, now, charge.io_bytes, steps, self.credit)
            .map_err(Error::from)
    }
}
