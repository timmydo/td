//! Private charging seam for decoding under job and aggregate header budgets.
use crate::{
    admission::work::{Charge, Meter, Stop},
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
