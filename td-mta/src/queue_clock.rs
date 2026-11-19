//! Process-local queue clock validation; status is not transport authority.
#[path = "queue_clock_control.rs"]
mod control;
pub(crate) use control::ClockControl;
#[path = "queue_clock_evidence.rs"]
pub(crate) mod evidence;
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

/// UTC/monotonic disagreement beyond this is a clock discontinuity, whatever
/// the retry floor.
const DISCONTINUITY_MS: u64 = 300_000;
static NEXT_VALIDATION: AtomicU64 = AtomicU64::new(1);

fn next_validation(counter: &AtomicU64) -> Result<u64, Error> {
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

/// The minimum retry interval (QUEUE.md section 4): the first automatic retry
/// delay and route backoff, and the stable monotonic interval before an
/// attempted recipient is redispatched or the clock confirmed. Five minutes
/// unless `serve --minimum-retry-seconds` chooses a shorter one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryFloor(u64);
impl RetryFloor {
    pub const DEFAULT: Self = Self(300_000);
    /// The shortest floor any process may choose. Persisted retry times are
    /// checked against it, so rows any permitted floor wrote stay valid.
    pub const LOWEST_MS: i64 = 1_000;
    pub fn from_seconds(seconds: u64) -> Option<Self> {
        (1..=300)
            .contains(&seconds)
            .then_some(Self(seconds * 1_000))
    }
    pub fn millis(self) -> u64 {
        self.0
    }
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
    /// Elapsed stable progress also admits cleanup during a dispatch hold.
    pub retention: bool,
}

/// Database observation and the latest UTC that cleanup may use to age data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionTime {
    pub(crate) observed: i64,
    pub(crate) cutoff: i64,
}

impl From<i64> for RetentionTime {
    fn from(observed: i64) -> Self {
        Self {
            observed,
            cutoff: observed,
        }
    }
}

/// Feed database UTC paired with this runtime's one monotonic tick domain.
/// A new guard holds expiry and age-based cleanup until trusted clock
/// evidence is restored or an operator confirms. Its ticks stay local.
pub struct Guard {
    floor: RetryFloor,
    anchor: Time,
    range: Option<evidence::Range>,
    retention_anchor: Option<Time>,
    last: Time,
    stable_since: u64,
    paused: bool,
    validation: u64,
    pub(crate) control: Option<std::sync::Arc<ClockControl>>,
    invalid: bool,
    next_expiry: Option<u64>,
    expiry_epoch: u64,
}

/// One admitted expiry batch of at most 100 recipients. Keep this token until
/// commit and validate it again before publication; it grants no SMTP authority.
pub struct Expiry {
    validation: u64,
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
        Self::with_retry_floor(sample, RetryFloor::DEFAULT)
    }

    pub fn with_retry_floor(sample: Time, floor: RetryFloor) -> Result<Self, Error> {
        if sample.utc_ms < 0 {
            return Err(Error::Invalid);
        }
        Ok(Self {
            floor,
            anchor: sample,
            range: None,
            retention_anchor: None,
            last: sample,
            stable_since: sample.monotonic.0,
            paused: false,
            validation: next_validation(&NEXT_VALIDATION)?,
            control: None,
            invalid: false,
            next_expiry: None,
            expiry_epoch: 0,
        })
    }

    fn hold(&mut self, sample: Time) {
        if self.range.take().is_some() {
            self.retention_anchor = Some(self.last);
        }
        if let Some(control) = &self.control {
            control.hold();
        }
        self.anchor = sample;
        self.stable_since = sample.monotonic.0;
        self.paused = true;
    }

    fn invalidate(&mut self) {
        self.invalid = true;
        if let Some(control) = &self.control {
            control.invalidate();
        }
    }

    pub fn observe(&mut self, sample: Time) -> Result<Status, Error> {
        if self.invalid || sample.utc_ms < 0 || sample.monotonic < self.last.monotonic {
            self.invalidate();
            return Err(Error::Invalid);
        }
        if sample.utc_ms < self.last.utc_ms {
            self.expiry_epoch = self.expiry_epoch.checked_add(1).ok_or_else(|| {
                self.invalidate();
                Error::Invalid
            })?;
        }
        // Check jumps and cumulative drift independently; either may cancel the other.
        let discontinuity = [self.anchor, self.last].iter().any(|prior| {
            let utc_elapsed = i128::from(sample.utc_ms) - i128::from(prior.utc_ms);
            let monotonic_elapsed = i128::from(sample.monotonic.0) - i128::from(prior.monotonic.0);
            (utc_elapsed - monotonic_elapsed).abs() > i128::from(DISCONTINUITY_MS)
        });
        let elapsed = sample.monotonic.0 - self.anchor.monotonic.0;
        if discontinuity
            || self
                .range
                .is_some_and(|range| !range.agrees(sample.utc_ms, elapsed))
        {
            self.hold(sample);
        }
        self.last = sample;
        if self.stable() {
            if let Some(control) = &self.control {
                control.stable(self.status().expiry);
            }
        }
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
        self.accept_confirmation(sample)?;
        if let Some(control) = &self.control {
            control.confirmed();
        }
        Ok(self.status())
    }

    fn accept_confirmation(&mut self, sample: Time) -> Result<(), Error> {
        let validation = next_validation(&NEXT_VALIDATION).inspect_err(|_| {
            self.invalidate();
        })?;
        self.paused = false;
        self.anchor = sample;
        self.range = Some(evidence::Range::point(sample.utc_ms));
        self.retention_anchor = Some(sample);
        self.validation = validation;
        Ok(())
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
            validation: self.validation,
            expiry_epoch: self.expiry_epoch,
            sample,
            deadline,
            invalid: false,
        })
    }

    /// The validation identity and this guard's original monotonic domain remain
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
            || self.validation != value.validation
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

    fn cleanup_cutoff(&self) -> Option<i64> {
        if self.invalid {
            return None;
        }
        let observed = self.last.utc_ms;
        if self.range.is_some() {
            return Some(observed);
        }
        let anchor = self.retention_anchor?;
        // A forward UTC step cannot age data by the size of the step.
        let elapsed = self.last.monotonic.0.checked_sub(anchor.monotonic.0)?;
        Some(observed.min(anchor.utc_ms.saturating_add_unsigned(elapsed)))
    }

    pub(crate) fn retention_time(&self) -> Option<RetentionTime> {
        if !self.stable() {
            return None;
        }
        Some(RetentionTime {
            observed: self.last.utc_ms,
            cutoff: self.cleanup_cutoff()?,
        })
    }

    fn stable(&self) -> bool {
        self.last.monotonic.0.saturating_sub(self.stable_since) >= self.floor.millis()
    }

    fn status(&self) -> Status {
        Status {
            fresh_dispatch: !self.paused,
            retry: !self.paused && self.stable(),
            expiry: self.range.is_some() && !self.paused && self.stable(),
            retention: self.retention_anchor.is_some() && self.stable(),
        }
    }
}

/// The foreground scheduler accepts the requested sample under the guard.
/// The durable write may outlive its validation; it only saves a cleanup cutoff.
pub(crate) fn observe_request(
    guard: &std::sync::Mutex<Guard>,
    sample: Time,
    save: impl FnOnce(i64) -> Result<(), Error>,
) -> Result<(Status, Option<RetentionTime>), Error> {
    {
        let mut clock = guard.lock().map_err(|_| Error::Invalid)?;
        let status = clock.observe(sample)?;
        if !clock
            .control
            .as_ref()
            .is_some_and(|control| control.begin_save())
        {
            return Ok((status, clock.retention_time()));
        }
        clock.accept_confirmation(sample)?;
    }
    let saved = save(sample.utc_ms);
    let mut clock = guard.lock().map_err(|_| Error::Invalid)?;
    saved.inspect_err(|_| clock.invalidate())?;
    if clock.invalid {
        return Err(Error::Invalid);
    }
    if let Some(control) = &clock.control {
        control.saved();
    }
    Ok((clock.status(), clock.retention_time()))
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
        retention: false,
    };
    const RETRY: Status = Status {
        retry: true,
        ..FRESH
    };
    const READY: Status = Status {
        fresh_dispatch: true,
        retry: true,
        expiry: true,
        retention: true,
    };
    const HELD: Status = Status {
        fresh_dispatch: false,
        retry: false,
        expiry: false,
        retention: false,
    };

    #[test]
    fn stable_startup_without_clock_evidence_admits_only_dispatch_and_retry() {
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
    fn control_never_takes_the_smtp_guard_and_a_slow_save_preserves_newer_requests() {
        use std::sync::{Arc, Mutex};
        let control = Arc::new(ClockControl::default());
        let mut clock = Guard::new(time(0, 0)).unwrap();
        clock.control = Some(control.clone());
        let guard = Mutex::new(clock);
        {
            let mut smtp = guard.try_lock().unwrap();
            assert!(!control.request());
            smtp.observe(time(300_000, 300_000)).unwrap();
            assert!(control.request());
            assert_eq!(control.status(), "requested");
        }
        // This query began before the request and cannot fulfill it.
        let mut saved = false;
        let (status, _) = observe_request(&guard, time(300_001, 300_001), |_| {
            saved = true;
            Ok(())
        })
        .unwrap();
        assert!(!saved && !status.expiry);
        control.start_sample();
        let (status, _) = observe_request(&guard, time(300_002, 300_002), |_| {
            let mut smtp = guard.try_lock().unwrap();
            assert_eq!(control.status(), "requested");
            assert!(control.request());
            smtp.observe(time(1_000_000, 300_003)).unwrap();
            assert_eq!(control.status(), "held");
            smtp.observe(time(1_300_000, 600_003)).unwrap();
            assert!(control.request()); // A newer request survives the old save.
            Ok(())
        })
        .unwrap();
        assert!(!status.expiry);
        assert_eq!(control.status(), "requested");
        control.start_sample();
        let (status, _) = observe_request(&guard, time(1_300_001, 600_004), |_| {
            guard
                .try_lock()
                .unwrap()
                .observe(time(1_300_003, 600_006))?;
            Ok(())
        })
        .unwrap();
        assert!(status.expiry);
        let clock = guard.lock().unwrap();
        assert_eq!(clock.anchor, time(1_300_001, 600_004));
        assert_eq!(clock.last, time(1_300_003, 600_006));
        assert_eq!(control.status(), "confirmed");
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
            assert_eq!(
                guard.observe(time(shifted + 300_000, 600_001)),
                Ok(Status {
                    retention: true,
                    ..HELD
                })
            );
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
    fn a_shorter_floor_shortens_the_stable_interval_but_not_the_discontinuity_bound() {
        assert_eq!(RetryFloor::from_seconds(0), None);
        assert_eq!(RetryFloor::from_seconds(301), None);
        assert_eq!(RetryFloor::from_seconds(300), Some(RetryFloor::DEFAULT));
        let floor = RetryFloor::from_seconds(30).unwrap();
        let mut guard = Guard::with_retry_floor(time(1_000_000, 0), floor).unwrap();
        assert_eq!(guard.observe(time(1_029_999, 29_999)), Ok(FRESH));
        assert_eq!(guard.confirm(time(1_029_999, 29_999)), Err(Error::Unstable));
        assert_eq!(guard.observe(time(1_030_000, 30_000)), Ok(RETRY));
        assert_eq!(guard.confirm(time(1_030_000, 30_000)), Ok(READY));
        // Drift within five minutes is no discontinuity, as at the default.
        assert_eq!(guard.observe(time(1_330_000, 30_001)), Ok(READY));
        assert_eq!(guard.observe(time(1_630_002, 30_002)), Ok(HELD));
        assert_eq!(guard.observe(time(1_660_001, 60_001)), Ok(HELD));
        assert_eq!(guard.confirm(time(1_660_001, 60_001)), Err(Error::Unstable));
        assert_eq!(guard.confirm(time(1_660_002, 60_002)), Ok(READY));
    }

    #[test]
    fn cleanup_progress_during_a_hold_excludes_forward_steps() {
        let mut guard = Guard::new(time(1_000_000, 0)).unwrap();
        guard.confirm(time(1_300_000, 300_000)).unwrap();
        guard.observe(time(9_000_000, 300_001)).unwrap();
        assert_eq!(guard.retention_time(), None);
        guard.observe(time(19_000_000, 300_002)).unwrap();
        let status = guard.observe(time(19_300_000, 600_002)).unwrap();
        assert!(!status.retry && !status.expiry && status.retention);
        assert_eq!(
            guard.retention_time(),
            Some(RetentionTime {
                observed: 19_300_000,
                cutoff: 1_600_002,
            })
        );
        guard.confirm(time(19_300_000, 600_002)).unwrap();
        assert_eq!(guard.retention_time(), Some(19_300_000.into()));
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
    fn expiry_admission_requires_stability_and_spaces_whole_batches() {
        let mut guard = Guard::new(time(1_000_000, 0)).unwrap();
        assert!(matches!(
            guard.admit_expiry(time(1_299_999, 299_999)),
            Err(Error::Unstable)
        ));
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
        assert_eq!(next_validation(&counter), Ok(u64::MAX - 1));
        assert_eq!(next_validation(&counter), Err(Error::Invalid));
        let mut guard = Guard::new(time(1_000_000, 0)).unwrap();
        guard.expiry_epoch = u64::MAX;
        assert_eq!(guard.observe(time(999_999, 1)), Err(Error::Invalid));
        assert_eq!(guard.confirm(time(1_300_000, 300_000)), Err(Error::Invalid));
    }
}
