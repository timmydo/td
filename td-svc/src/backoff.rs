//! Restart backoff — exponential with a cap, per DESIGN.md §6.
//!
//! No jitter. Jitter exists to spread a herd of clients across a shared
//! server; local supervision has no herd, and a dependency-free crate has no
//! RNG to draw from anyway.

use std::time::Duration;

/// First retry delay. PID 1's flat one second is a long time to wait for a
/// service that failed on a transient; the cap is what protects the console.
pub const BASE: Duration = Duration::from_millis(100);
/// The ceiling. A job that can never start settles here rather than scrolling
/// a serial console until the diagnostics that would explain it are gone.
pub const CAP: Duration = Duration::from_secs(300);
/// A run shorter than this counts as a fast failure. A service that stays up
/// this long has its failure count reset.
pub const MIN_UPTIME: Duration = Duration::from_secs(1);

/// How long to wait before restart number `fast_failures` (1-based).
///
/// `0` is not a meaningful input — a service with no consecutive fast failures
/// is not waiting — and is treated as the first retry rather than as an
/// instant one, so a caller that is off by one still throttles.
pub fn delay(fast_failures: u32) -> Duration {
    let steps = fast_failures.saturating_sub(1).min(u32::BITS - 1);
    match BASE.checked_mul(1u32.wrapping_shl(steps)) {
        Some(d) if d < CAP => d,
        // Both the overflow arm and the past-the-cap arm land here, which is
        // the same answer: hold at the ceiling.
        _ => CAP,
    }
}

/// Is this failure worth a console line? The first one is, and so is the
/// transition into the capped hold — after that the hold is silent, because
/// the whole point of the cap is to stop filling the console.
pub fn should_report(fast_failures: u32) -> bool {
    fast_failures <= 1 || (delay(fast_failures) == CAP && delay(fast_failures - 1) != CAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_delay_doubles_from_base_until_it_reaches_the_cap() {
        assert_eq!(delay(1), Duration::from_millis(100));
        assert_eq!(delay(2), Duration::from_millis(200));
        assert_eq!(delay(3), Duration::from_millis(400));
        assert_eq!(delay(4), Duration::from_millis(800));
        assert_eq!(delay(5), Duration::from_millis(1600));
    }

    /// The cap is the property that matters: a service that can never start
    /// must not keep the supervisor busy or the console full.
    #[test]
    fn the_delay_never_exceeds_the_cap_however_many_failures() {
        for n in 0..=u32::BITS + 8 {
            assert!(delay(n) <= CAP, "delay({n}) exceeded the cap");
        }
        assert_eq!(delay(u32::MAX), CAP);
    }

    /// A large shift count is UB-adjacent in C and a panic in debug Rust; this
    /// asserts the saturating path rather than trusting the arithmetic.
    #[test]
    fn a_huge_failure_count_does_not_overflow() {
        assert_eq!(delay(1000), CAP);
        assert_eq!(delay(u32::BITS), CAP);
    }

    #[test]
    fn zero_is_treated_as_the_first_retry_not_as_no_wait() {
        assert_eq!(delay(0), BASE);
    }

    /// Reporting is the first failure and the moment it gives up, and nothing
    /// in between or after — one line when it breaks, one when it stops trying.
    #[test]
    fn reporting_covers_the_first_failure_and_the_move_into_the_hold() {
        assert!(should_report(1));
        assert!(!should_report(2));
        assert!(!should_report(3));
        let mut first_capped = 0;
        for n in 1..64 {
            if delay(n) == CAP {
                first_capped = n;
                break;
            }
        }
        assert!(first_capped > 0);
        assert!(should_report(first_capped));
        assert!(!should_report(first_capped + 1));
        assert!(!should_report(first_capped + 50));
    }
}
