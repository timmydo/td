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

    fn sample_at(&self, utc: SystemTime, monotonic: Instant) -> Result<Time, Error> {
        let elapsed = monotonic
            .checked_duration_since(self.origin)
            .ok_or(Error::Invalid)?;
        let utc = utc.duration_since(UNIX_EPOCH).map_err(|_| Error::Invalid)?;
        Ok(Time {
            utc_ms: i64::try_from(utc.as_millis()).map_err(|_| Error::Invalid)?,
            monotonic: Tick(u64::try_from(elapsed.as_millis()).map_err(|_| Error::Invalid)?),
        })
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
    fn runtime_clock_keeps_utc_and_monotonic_domains_separate() {
        let clock = RuntimeClock::new();
        let future = clock
            .origin
            .checked_add(Duration::from_millis(1234))
            .unwrap();
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
