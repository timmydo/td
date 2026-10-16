//! Process-local queue clock validation; status is not transport authority.
use crate::ports::Time;
use std::sync::atomic::{AtomicU64, Ordering};

/// Pair authoritative database UTC with the shared process monotonic domain.
pub(crate) fn admit_database(
    clock: &dyn crate::ports::Clock,
    guard: &std::sync::Mutex<Guard>,
    utc_ms: i64,
    retry: bool,
) -> Result<(), Error> {
    let mut guard = guard.try_lock().map_err(|_| Error::Invalid)?;
    let monotonic = clock.sample().map_err(|_| Error::Invalid)?.monotonic;
    let status = guard.observe(Time { utc_ms, monotonic })?;
    if if retry {
        status.retry
    } else {
        status.fresh_dispatch
    } {
        Ok(())
    } else {
        Err(Error::Unstable)
    }
}

const GUARD_MS: u64 = 300_000;
static NEXT_CONFIRMATION: AtomicU64 = AtomicU64::new(1);

fn next_confirmation(counter: &AtomicU64) -> Result<u64, Error> {
    let mut current = counter.load(Ordering::Relaxed);
    for _ in 0..32 {
        let next = current.checked_add(1).ok_or(Error::Invalid)?;
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Ok(current),
            Err(actual) => current = actual,
        }
    }
    Err(Error::Invalid)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Unstable,
    RateLimited,
    Expired,
}

/// A snapshot for scheduling/health. Recheck before each operation; no status
/// substitutes for current database ownership, route health or due times.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Status {
    pub fresh_dispatch: bool,
    pub retry: bool,
    pub expiry: bool,
}

/// Feed database UTC paired with this runtime's one monotonic tick domain.
/// Recreate at process startup; never persist or transfer its local ticks.
pub struct Guard {
    anchor: Time,
    last: Time,
    stable_since: u64,
    paused: bool,
    confirmation: Option<u64>,
    invalid: bool,
    next_expiry: Option<u64>,
    expiry_epoch: u64,
}

/// One admitted expiry batch of at most 100 recipients. Keep this token until
/// commit and validate it again before publication; it grants no SMTP authority.
pub struct Expiry {
    confirmation: u64,
    expiry_epoch: u64,
    sample: Time,
    deadline: u64,
    invalid: bool,
}
impl Expiry {
    pub fn cutoff_utc_ms(&self) -> i64 {
        self.sample.utc_ms
    }
}

impl Guard {
    pub fn new(sample: Time) -> Result<Self, Error> {
        if sample.utc_ms < 0 {
            return Err(Error::Invalid);
        }
        Ok(Self {
            anchor: sample,
            last: sample,
            stable_since: sample.monotonic.0,
            paused: false,
            confirmation: None,
            invalid: false,
            next_expiry: None,
            expiry_epoch: 0,
        })
    }

    fn hold(&mut self, sample: Time) {
        self.anchor = sample;
        self.stable_since = sample.monotonic.0;
        self.paused = true;
        self.confirmation = None;
    }

    pub fn observe(&mut self, sample: Time) -> Result<Status, Error> {
        if self.invalid || sample.utc_ms < 0 || sample.monotonic < self.last.monotonic {
            self.invalid = true;
            return Err(Error::Invalid);
        }
        if sample.utc_ms < self.last.utc_ms {
            self.expiry_epoch = self.expiry_epoch.checked_add(1).ok_or_else(|| {
                self.invalid = true;
                Error::Invalid
            })?;
        }
        // Check jumps and cumulative drift independently; either may cancel the other.
        let discontinuity = [self.anchor, self.last].iter().any(|prior| {
            let utc_elapsed = i128::from(sample.utc_ms) - i128::from(prior.utc_ms);
            let monotonic_elapsed = i128::from(sample.monotonic.0) - i128::from(prior.monotonic.0);
            (utc_elapsed - monotonic_elapsed).abs() > i128::from(GUARD_MS)
        });
        if discontinuity {
            self.hold(sample);
        }
        self.last = sample;
        Ok(self.status())
    }

    /// A primary change invalidates earlier validation even without a UTC jump.
    pub fn primary_changed(&mut self, sample: Time) -> Result<Status, Error> {
        self.observe(sample)?;
        self.hold(sample);
        Ok(self.status())
    }

    /// Call only for an explicitly authorized queue clock-confirm operation.
    /// Stable elapsed time alone never clears a discontinuity or expiry hold.
    pub fn confirm(&mut self, sample: Time) -> Result<Status, Error> {
        self.observe(sample)?;
        if !self.stable() {
            return Err(Error::Unstable);
        }
        let confirmation = next_confirmation(&NEXT_CONFIRMATION).inspect_err(|_| {
            self.invalid = true;
        })?;
        self.paused = false;
        self.confirmation = Some(confirmation);
        Ok(self.status())
    }

    /// Admit at most one expiry batch per monotonic second on this shared guard.
    /// Failed batches still consume the slot; callers must not refund it.
    pub fn admit_expiry(&mut self, sample: Time) -> Result<Expiry, Error> {
        if !self.observe(sample)?.expiry {
            return Err(Error::Unstable);
        }
        if self
            .next_expiry
            .is_some_and(|next| sample.monotonic.0 < next)
        {
            return Err(Error::RateLimited);
        }
        let next = sample.monotonic.0.checked_add(1000).ok_or(Error::Invalid)?;
        let deadline = sample
            .monotonic
            .0
            .checked_add(10_000)
            .ok_or(Error::Invalid)?;
        self.next_expiry = Some(next);
        Ok(Expiry {
            confirmation: self.confirmation.ok_or(Error::Unstable)?,
            expiry_epoch: self.expiry_epoch,
            sample,
            deadline,
            invalid: false,
        })
    }

    /// Fresh confirmation and this guard's original monotonic domain remain
    /// mandatory; elapsed deadlines, backward UTC or a new hold retire the token.
    pub fn validate_expiry(&mut self, value: &mut Expiry, sample: Time) -> Result<(), Error> {
        let status = match self.observe(sample) {
            Ok(status) => status,
            Err(error) => {
                value.invalid = true;
                return Err(error);
            }
        };
        if value.invalid
            || !status.expiry
            || self.confirmation != Some(value.confirmation)
            || self.expiry_epoch != value.expiry_epoch
            || sample.monotonic.0 >= value.deadline
            || sample.monotonic < value.sample.monotonic
            || sample.utc_ms < value.sample.utc_ms
        {
            value.invalid = true;
            return Err(Error::Expired);
        }
        Ok(())
    }

    fn stable(&self) -> bool {
        self.last.monotonic.0.saturating_sub(self.stable_since) >= GUARD_MS
    }

    fn status(&self) -> Status {
        Status {
            fresh_dispatch: !self.paused,
            retry: !self.paused && self.stable(),
            expiry: !self.paused && self.confirmation.is_some(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::ports::Tick;

    fn time(utc_ms: i64, monotonic: u64) -> Time {
        Time {
            utc_ms,
            monotonic: Tick(monotonic),
        }
    }
    const FRESH: Status = Status {
        fresh_dispatch: true,
        retry: false,
        expiry: false,
    };
    const RETRY: Status = Status {
        fresh_dispatch: true,
        retry: true,
        expiry: false,
    };
    const READY: Status = Status {
        fresh_dispatch: true,
        retry: true,
        expiry: true,
    };
    const HELD: Status = Status {
        fresh_dispatch: false,
        retry: false,
        expiry: false,
    };

    #[test]
    fn startup_retry_interval_and_expiry_confirmation_are_separate() {
        let mut guard = Guard::new(time(1_000_000, 700)).unwrap();
        assert_eq!(guard.observe(time(1_000_000, 700)), Ok(FRESH));
        assert_eq!(
            guard.confirm(time(1_299_999, 300_699)),
            Err(Error::Unstable)
        );
        assert_eq!(guard.observe(time(1_300_000, 300_700)), Ok(RETRY));
        assert_eq!(guard.observe(time(9_000_000, 8_000_700)), Ok(RETRY));
        assert_eq!(guard.confirm(time(9_000_000, 8_000_700)), Ok(READY));
        // A new process cannot inherit either elapsed ticks or confirmation.
        let mut restarted = Guard::new(time(9_000_000, 0)).unwrap();
        assert_eq!(restarted.observe(time(9_000_000, 0)), Ok(FRESH));
    }

    #[test]
    fn both_clock_jump_directions_hold_until_fresh_explicit_confirmation() {
        for jump in [-300_001, 300_001] {
            let mut guard = Guard::new(time(1_000_000, 0)).unwrap();
            assert_eq!(guard.confirm(time(1_300_000, 300_000)), Ok(READY));
            let shifted = 1_300_001 + jump;
            assert_eq!(guard.observe(time(shifted, 300_001)), Ok(HELD));
            assert_eq!(
                guard.confirm(time(shifted + 299_999, 600_000)),
                Err(Error::Unstable)
            );
            assert_eq!(guard.observe(time(shifted + 300_000, 600_001)), Ok(HELD));
            assert_eq!(guard.confirm(time(shifted + 300_000, 600_001)), Ok(READY));
        }
    }

    #[test]
    fn exact_threshold_and_cumulative_drift_use_one_stable_anchor() {
        let mut guard = Guard::new(time(1_000_000, 0)).unwrap();
        assert_eq!(guard.observe(time(1_200_000, 100_000)), Ok(FRESH));
        assert_eq!(guard.observe(time(1_400_000, 200_000)), Ok(FRESH));
        assert_eq!(guard.observe(time(1_600_000, 300_000)), Ok(RETRY));
        assert_eq!(guard.observe(time(1_600_002, 300_001)), Ok(HELD));
        // Another jump restarts the entire stable interval.
        assert_eq!(guard.observe(time(2_200_003, 600_001)), Ok(HELD));
        assert_eq!(
            guard.confirm(time(2_200_004, 600_002)),
            Err(Error::Unstable)
        );
    }

    #[test]
    fn jumps_cannot_hide_by_canceling_earlier_drift() {
        for drift in [-299_999, 299_999] {
            let mut guard = Guard::new(time(1_000_000, 0)).unwrap();
            assert_eq!(guard.confirm(time(1_300_000 + drift, 300_000)), Ok(READY));
            let reversal = if drift > 0 { -300_001 } else { 300_001 };
            let shifted = 1_300_001 + drift + reversal;
            assert_eq!(guard.observe(time(shifted, 300_001)), Ok(HELD));
            assert_eq!(
                guard.confirm(time(shifted + 299_999, 600_000)),
                Err(Error::Unstable)
            );
            assert_eq!(guard.confirm(time(shifted + 300_000, 600_001)), Ok(READY));
        }
    }

    #[test]
    fn primary_change_and_invalid_samples_fail_closed() {
        let mut guard = Guard::new(time(1_000_000, 0)).unwrap();
        assert_eq!(guard.confirm(time(1_300_000, 300_000)), Ok(READY));
        assert_eq!(guard.primary_changed(time(1_300_001, 300_001)), Ok(HELD));
        assert_eq!(
            guard.confirm(time(1_600_000, 600_000)),
            Err(Error::Unstable)
        );
        assert_eq!(guard.confirm(time(1_600_001, 600_001)), Ok(READY));
        assert_eq!(guard.observe(time(1_600_002, 600_000)), Err(Error::Invalid));
        assert_eq!(guard.confirm(time(1_900_002, 900_002)), Err(Error::Invalid));
        assert_eq!(
            guard.primary_changed(time(1_900_003, 900_003)),
            Err(Error::Invalid)
        );
        let mut guard = Guard::new(time(0, 0)).unwrap();
        assert_eq!(guard.observe(time(-1, 1)), Err(Error::Invalid));
        assert_eq!(guard.observe(time(1, 2)), Err(Error::Invalid));
        assert!(matches!(Guard::new(time(-1, 0)), Err(Error::Invalid)));
    }

    #[test]
    fn extreme_time_values_do_not_overflow_or_renew_a_guard() {
        let mut guard = Guard::new(time(0, 0)).unwrap();
        assert_eq!(guard.observe(time(i64::MAX, u64::MAX)), Ok(HELD));
        assert_eq!(
            guard.confirm(time(i64::MAX, u64::MAX)),
            Err(Error::Unstable)
        );
        let mut guard = Guard::new(time(i64::MAX, u64::MAX - 299_999)).unwrap();
        assert_eq!(
            guard.confirm(time(i64::MAX, u64::MAX)),
            Err(Error::Unstable)
        );
    }
    #[test]
    fn expiry_admission_requires_confirmation_and_spaces_whole_batches() {
        let mut guard = Guard::new(time(1_000_000, 0)).unwrap();
        assert!(matches!(
            guard.admit_expiry(time(1_300_000, 300_000)),
            Err(Error::Unstable)
        ));
        guard.confirm(time(1_300_000, 300_000)).unwrap();
        let mut first = guard.admit_expiry(time(1_300_000, 300_000)).unwrap();
        assert_eq!(first.cutoff_utc_ms(), 1_300_000);
        assert!(matches!(
            guard.admit_expiry(time(1_300_999, 300_999)),
            Err(Error::RateLimited)
        ));
        let mut second = guard.admit_expiry(time(1_301_000, 301_000)).unwrap();
        assert_eq!(
            guard.validate_expiry(&mut first, time(1_309_999, 309_999)),
            Ok(())
        );
        assert_eq!(
            guard.validate_expiry(&mut first, time(1_310_000, 310_000)),
            Err(Error::Expired)
        );
        assert_eq!(
            guard.validate_expiry(&mut second, time(1_310_000, 310_000)),
            Ok(())
        );
    }

    #[test]
    fn expiry_tokens_do_not_survive_holds_or_backward_utc() {
        let mut guard = Guard::new(time(1_000_000, 0)).unwrap();
        guard.confirm(time(1_300_000, 300_000)).unwrap();
        let mut token = guard.admit_expiry(time(1_300_000, 300_000)).unwrap();
        assert_eq!(
            guard.validate_expiry(&mut token, time(1_299_999, 300_001)),
            Err(Error::Expired)
        );
        assert_eq!(
            guard.validate_expiry(&mut token, time(1_300_001, 300_002)),
            Err(Error::Expired)
        );
        guard.primary_changed(time(1_300_002, 300_002)).unwrap();
        assert_eq!(
            guard.validate_expiry(&mut token, time(1_300_003, 300_003)),
            Err(Error::Expired)
        );
        guard.confirm(time(1_600_002, 600_002)).unwrap();
        assert_eq!(
            guard.validate_expiry(&mut token, time(1_600_002, 600_002)),
            Err(Error::Expired)
        );
        let mut token = guard.admit_expiry(time(1_600_002, 600_002)).unwrap();
        assert_eq!(
            guard.validate_expiry(&mut token, time(1_900_004, 600_003)),
            Err(Error::Expired)
        );
    }

    #[test]
    fn expiry_admission_arithmetic_cannot_wrap() {
        let mut guard = Guard::new(time(1_000_000, u64::MAX - 300_000)).unwrap();
        guard.confirm(time(1_300_000, u64::MAX)).unwrap();
        assert!(matches!(
            guard.admit_expiry(time(1_300_000, u64::MAX)),
            Err(Error::Invalid)
        ));
    }
    #[test]
    fn expiry_tokens_bind_one_confirmation_and_cannot_cross_guards() {
        let mut first = Guard::new(time(1_000_000, 0)).unwrap();
        let mut second = Guard::new(time(1_000_000, 0)).unwrap();
        first.confirm(time(1_300_000, 300_000)).unwrap();
        second.confirm(time(1_300_000, 300_000)).unwrap();
        let mut token = first.admit_expiry(time(1_300_000, 300_000)).unwrap();
        assert_eq!(
            second.validate_expiry(&mut token, time(1_300_001, 300_001)),
            Err(Error::Expired)
        );
        assert_eq!(
            first.validate_expiry(&mut token, time(1_300_001, 300_001)),
            Err(Error::Expired)
        );
        let mut token = first.admit_expiry(time(1_301_000, 301_000)).unwrap();
        first.confirm(time(1_301_001, 301_001)).unwrap();
        assert_eq!(
            first.validate_expiry(&mut token, time(1_301_001, 301_001)),
            Err(Error::Expired)
        );
    }
    #[test]
    fn every_observed_utc_regression_retires_existing_expiry_tokens() {
        for intermediate_validation in [false, true] {
            let mut guard = Guard::new(time(1_000_000, 0)).unwrap();
            guard.confirm(time(1_300_000, 300_000)).unwrap();
            let mut token = guard.admit_expiry(time(1_300_000, 300_000)).unwrap();
            if intermediate_validation {
                guard
                    .validate_expiry(&mut token, time(1_305_000, 305_000))
                    .unwrap();
                assert_eq!(
                    guard.validate_expiry(&mut token, time(1_304_000, 306_000)),
                    Err(Error::Expired)
                );
            } else {
                assert_eq!(guard.observe(time(1_299_999, 300_001)), Ok(READY));
                assert_eq!(guard.observe(time(1_300_002, 300_002)), Ok(READY));
                assert_eq!(
                    guard.validate_expiry(&mut token, time(1_300_003, 300_003)),
                    Err(Error::Expired)
                );
            }
            assert_eq!(
                guard.validate_expiry(&mut token, time(1_307_000, 307_000)),
                Err(Error::Expired)
            );
        }
    }
    #[test]
    fn expiry_identity_and_regression_counters_refuse_exhaustion() {
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(next_confirmation(&counter), Ok(u64::MAX - 1));
        assert_eq!(next_confirmation(&counter), Err(Error::Invalid));
        let mut guard = Guard::new(time(1_000_000, 0)).unwrap();
        guard.expiry_epoch = u64::MAX;
        assert_eq!(guard.observe(time(999_999, 1)), Err(Error::Invalid));
        assert_eq!(guard.confirm(time(1_300_000, 300_000)), Err(Error::Invalid));
    }
}
