//! Process-local queue clock validation; status is not transport authority.
use crate::ports::Time;

const GUARD_MS: u64 = 300_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Unstable,
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
    confirmed: bool,
    invalid: bool,
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
            confirmed: false,
            invalid: false,
        })
    }

    fn hold(&mut self, sample: Time) {
        self.anchor = sample;
        self.stable_since = sample.monotonic.0;
        self.paused = true;
        self.confirmed = false;
    }

    pub fn observe(&mut self, sample: Time) -> Result<Status, Error> {
        if self.invalid || sample.utc_ms < 0 || sample.monotonic < self.last.monotonic {
            self.invalid = true;
            return Err(Error::Invalid);
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
        self.paused = false;
        self.confirmed = true;
        Ok(self.status())
    }

    fn stable(&self) -> bool {
        self.last.monotonic.0.saturating_sub(self.stable_since) >= GUARD_MS
    }

    fn status(&self) -> Status {
        Status {
            fresh_dispatch: !self.paused,
            retry: !self.paused && self.stable(),
            expiry: !self.paused && self.confirmed,
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
}
