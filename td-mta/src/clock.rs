//! Process-local monotonic time and checked conversion for the shared TLS clock.
use crate::ports::{Clock, Error, Tick, Time};
use std::{
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

/// One origin per runtime. Share this clock across all deadline consumers;
/// independently constructed origins and persisted ticks are not comparable.
pub struct RuntimeClock {
    origin: Instant,
}

impl RuntimeClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }

    /// Convert only deadlines created in this runtime's tick domain. Keeping
    /// the original origin avoids renewing a budget at a blocking adapter.
    pub fn deadline_instant(&self, deadline: crate::ports::Deadline) -> Result<Instant, Error> {
        self.origin
            .checked_add(std::time::Duration::from_millis(deadline.tick().0))
            .ok_or(Error::Invalid)
    }

    /// Round toward the origin so a converted cap can only become earlier.
    pub(crate) fn deadline_before(&self, limit: Instant) -> Result<crate::ports::Deadline, Error> {
        let elapsed = limit
            .checked_duration_since(self.origin)
            .ok_or(Error::Invalid)?;
        let millis = u64::try_from(elapsed.as_millis()).map_err(|_| Error::Invalid)?;
        crate::ports::Deadline::after(Tick(0), millis)
    }

    fn sample_at(&self, utc: SystemTime, monotonic: Instant) -> Result<Time, Error> {
        let monotonic = self.monotonic_at(monotonic)?;
        let utc = utc.duration_since(UNIX_EPOCH).map_err(|_| Error::Invalid)?;
        Ok(Time {
            utc_ms: i64::try_from(utc.as_millis()).map_err(|_| Error::Invalid)?,
            monotonic,
        })
    }

    fn monotonic_at(&self, now: Instant) -> Result<Tick, Error> {
        let elapsed = now
            .checked_duration_since(self.origin)
            .ok_or(Error::Invalid)?;
        Ok(Tick(
            u64::try_from(elapsed.as_millis()).map_err(|_| Error::Invalid)?,
        ))
    }
}

impl Default for RuntimeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for RuntimeClock {
    fn sample(&self) -> Result<Time, Error> {
        self.sample_at(SystemTime::now(), Instant::now())
    }
    fn monotonic(&self) -> Result<Tick, Error> {
        self.monotonic_at(Instant::now())
    }
}

/// Cold-created source for td_crypto::ClockHandle. The injected mail clock
/// must perform bounded, nonblocking work and obey that handle's callback rules.
pub struct TlsClockSource {
    clock: Arc<dyn Clock>,
}

impl TlsClockSource {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self { clock }
    }
}

impl td_crypto::UtcClock for TlsClockSource {
    fn now(&self) -> Option<u64> {
        let sample = self.clock.sample().ok()?;
        u64::try_from(sample.utc_ms)
            .ok()
            .map(|millis| millis / 1000)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
        time::Duration,
    };
    use td_crypto::UtcClock;

    #[test]
    fn converted_cap_rounds_earlier_and_rejects_before_origin() {
        let clock = RuntimeClock::new();
        let limit = clock.origin + Duration::from_micros(1_234_999);
        let deadline = clock.deadline_before(limit).unwrap();
        assert_eq!(deadline.tick(), Tick(1234));
        assert!(clock.deadline_instant(deadline).unwrap() <= limit);
        assert_eq!(
            clock.deadline_before(clock.origin - Duration::from_nanos(1)),
            Err(Error::Invalid)
        );
    }

    #[test]
    fn runtime_clock_keeps_utc_and_monotonic_domains_separate() {
        let clock = RuntimeClock::new();
        let future = clock
            .origin
            .checked_add(Duration::from_millis(1234))
            .unwrap();
        assert_eq!(
            clock.deadline_instant(crate::ports::Deadline::after(Tick(1000), 234).unwrap()),
            Ok(future)
        );
        assert_eq!(
            clock.deadline_instant(crate::ports::Deadline::after(Tick(0), 0).unwrap()),
            Ok(clock.origin)
        );
        assert_eq!(
            clock.sample_at(UNIX_EPOCH + Duration::from_millis(5678), future),
            Ok(Time {
                utc_ms: 5678,
                monotonic: Tick(1234)
            })
        );
        assert_eq!(
            clock.sample_at(UNIX_EPOCH, future),
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(1234)
            })
        );
        assert_eq!(
            clock.sample_at(UNIX_EPOCH - Duration::from_nanos(1), future),
            Err(Error::Invalid)
        );
        let backwards = clock.origin.checked_sub(Duration::from_millis(1)).unwrap();
        assert_eq!(clock.sample_at(UNIX_EPOCH, backwards), Err(Error::Invalid));
        assert_eq!(clock.monotonic_at(backwards), Err(Error::Invalid));
        assert_eq!(clock.monotonic_at(future), Ok(Tick(1234)));
        let before = clock.monotonic().unwrap();
        let combined = clock.sample().unwrap();
        let after = clock.monotonic().unwrap();
        assert!(before <= combined.monotonic && combined.monotonic <= after);
        let large = UNIX_EPOCH
            .checked_add(Duration::from_millis(i64::MAX as u64 + 1))
            .unwrap();
        assert_eq!(clock.sample_at(large, future), Err(Error::Invalid));
        let first = clock.sample().unwrap();
        let second = clock.sample().unwrap();
        assert!(second.monotonic >= first.monotonic);
        assert!(first.utc_ms > 0);
    }

    struct Injected {
        value: Mutex<Result<Time, Error>>,
        calls: AtomicUsize,
    }
    impl Clock for Injected {
        fn sample(&self) -> Result<Time, Error> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            *self.value.lock().unwrap()
        }
    }

    #[test]
    fn default_monotonic_preserves_injected_clock_errors_and_ticks() {
        let clock = Injected {
            value: Mutex::new(Err(Error::Deadline)),
            calls: AtomicUsize::new(0),
        };
        assert_eq!(clock.monotonic(), Err(Error::Deadline));
        *clock.value.lock().unwrap() = Ok(Time {
            utc_ms: 9999,
            monotonic: Tick(42),
        });
        assert_eq!(clock.monotonic(), Ok(Tick(42)));
        assert_eq!(clock.calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn tls_clock_conversion_never_falls_back_after_refusal() {
        let clock = Arc::new(Injected {
            value: Mutex::new(Err(Error::Invalid)),
            calls: AtomicUsize::new(0),
        });
        let source = TlsClockSource::new(clock.clone());
        assert_eq!(clock.calls.load(Ordering::Relaxed), 0);
        for (millis, expected) in [
            (-1000, None),
            (-1, None),
            (0, Some(0)),
            (999, Some(0)),
            (1000, Some(1)),
            (1234, Some(1)),
            (i64::MAX, Some(i64::MAX as u64 / 1000)),
        ] {
            *clock.value.lock().unwrap() = Ok(Time {
                utc_ms: millis,
                monotonic: Tick(u64::MAX),
            });
            assert_eq!(source.now(), expected);
        }
        *clock.value.lock().unwrap() = Err(Error::Deadline);
        assert_eq!(source.now(), None);
        let shared = td_crypto::ClockHandle::new(source);
        assert_eq!(shared.now(), Err(td_crypto::TlsError::Clock));
        *clock.value.lock().unwrap() = Ok(Time {
            utc_ms: 2000,
            monotonic: Tick(0),
        });
        assert_eq!(shared.now(), Ok(2));
    }
}
