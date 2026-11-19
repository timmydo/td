//! The control socket reads a clock summary and submits requests without
//! taking the mutex used by nonblocking SMTP clock admission.
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Default)]
pub(crate) struct ClockControl(AtomicU8);

const WAIT: u8 = 0;
const EARLY: u8 = 1;
const ELIGIBLE: u8 = 2;
const ELIGIBLE_REFUSED: u8 = 3;
const CONFIRMED: u8 = 4;
const REQUESTED: u8 = 5;
const SAVING: u8 = 6;
const INVALID: u8 = 7;
const SAMPLING: u8 = 8;

impl ClockControl {
    pub(crate) fn request(&self) -> bool {
        let prior = self
            .0
            .try_update(Ordering::AcqRel, Ordering::Acquire, |s| match s {
                WAIT => Some(EARLY),
                ELIGIBLE | ELIGIBLE_REFUSED | CONFIRMED => Some(REQUESTED),
                _ => None,
            });
        matches!(
            prior,
            Ok(ELIGIBLE | ELIGIBLE_REFUSED | CONFIRMED) | Err(REQUESTED | SAMPLING | SAVING)
        )
    }

    pub(crate) fn status(&self) -> &'static str {
        match self.0.load(Ordering::Acquire) {
            EARLY | ELIGIBLE_REFUSED | INVALID => "refused",
            CONFIRMED => "confirmed",
            REQUESTED | SAMPLING | SAVING => "requested",
            _ => "held",
        }
    }

    pub(super) fn stable(&self, expiry: bool) {
        let _ = self
            .0
            .try_update(Ordering::AcqRel, Ordering::Acquire, |s| match s {
                EARLY | WAIT if expiry => Some(CONFIRMED),
                EARLY => Some(ELIGIBLE_REFUSED),
                WAIT => Some(ELIGIBLE),
                _ => None,
            });
    }
    pub(super) fn hold(&self) {
        let _ = self
            .0
            .try_update(Ordering::AcqRel, Ordering::Acquire, |s| match s {
                EARLY | WAIT | INVALID => None,
                REQUESTED | SAMPLING | ELIGIBLE_REFUSED => Some(EARLY),
                _ => Some(WAIT),
            });
    }
    /// A request arriving after the query starts needs the following sample.
    pub(crate) fn start_sample(&self) {
        let _ = self
            .0
            .compare_exchange(REQUESTED, SAMPLING, Ordering::AcqRel, Ordering::Acquire);
    }
    pub(super) fn begin_save(&self) -> bool {
        self.0
            .compare_exchange(SAMPLING, SAVING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    pub(super) fn saved(&self) {
        let _ = self
            .0
            .compare_exchange(SAVING, CONFIRMED, Ordering::AcqRel, Ordering::Acquire);
    }
    pub(super) fn confirmed(&self) {
        self.0.store(CONFIRMED, Ordering::Release);
    }
    pub(super) fn invalidate(&self) {
        self.0.store(INVALID, Ordering::Release);
    }
}
