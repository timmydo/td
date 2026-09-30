//! Each connection retains its own sticky backend callback failure.
use crate::{ClockHandle, TlsError};
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};
pub(super) struct SessionClock {
    source: Arc<ClockHandle>,
    failure: AtomicU8,
}
impl SessionClock {
    pub(super) fn new(source: Arc<ClockHandle>) -> Self {
        Self {
            source,
            failure: AtomicU8::new(0),
        }
    }
    pub(super) fn now(&self) -> Result<u64, TlsError> {
        self.check()?;
        let value = self.source.now().inspect_err(|error| {
            let code = if *error == TlsError::Clock { 1 } else { 2 };
            self.failure.fetch_max(code, Ordering::SeqCst);
        })?;
        self.check()?;
        Ok(value)
    }
    // No new callback: a later successful poll must not hide transient None.
    pub(super) fn check(&self) -> Result<(), TlsError> {
        self.source.health()?;
        match self.failure.load(Ordering::SeqCst) {
            0 => Ok(()),
            1 => Err(TlsError::Clock),
            _ => Err(TlsError::Crypto),
        }
    }
}
impl std::fmt::Debug for SessionClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TlsSessionClock(<redacted>)")
    }
}
impl rustls::time_provider::TimeProvider for SessionClock {
    fn current_time(&self) -> Option<rustls::pki_types::UnixTime> {
        self.now().ok().map(|seconds| {
            rustls::pki_types::UnixTime::since_unix_epoch(std::time::Duration::from_secs(seconds))
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::UtcClock;
    use rustls::time_provider::TimeProvider;
    use std::sync::atomic::AtomicUsize;
    #[derive(Clone)]
    struct Source {
        calls: Arc<AtomicUsize>,
        mode: Arc<AtomicU8>,
    }
    impl UtcClock for Source {
        fn now(&self) -> Option<u64> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.mode.load(Ordering::SeqCst) {
                0 => Some(100),
                1 => None,
                _ => panic!("synthetic callback panic"),
            }
        }
    }
    #[test]
    fn observers_isolate_transient_errors_and_share_retirement() {
        let source = Source {
            calls: Arc::new(AtomicUsize::new(0)),
            mode: Arc::new(AtomicU8::new(0)),
        };
        let shared = Arc::new(ClockHandle::new(source.clone()));
        let first = SessionClock::new(shared.clone());
        let second = SessionClock::new(shared.clone());
        assert_eq!(first.now(), Ok(100));
        source.mode.store(1, Ordering::SeqCst);
        assert!(first.current_time().is_none());
        source.mode.store(0, Ordering::SeqCst);
        assert_eq!(shared.now(), Ok(100));
        assert_eq!(second.now(), Ok(100));
        let calls = source.calls.load(Ordering::SeqCst);
        assert_eq!(first.check(), Err(TlsError::Clock));
        assert_eq!(first.now(), Err(TlsError::Clock));
        assert_eq!(second.check(), Ok(()));
        assert_eq!(source.calls.load(Ordering::SeqCst), calls);
        source.mode.store(2, Ordering::SeqCst);
        assert_eq!(second.now(), Err(TlsError::Crypto));
        assert_eq!(first.check(), Err(TlsError::Crypto));
        assert_eq!(SessionClock::new(shared).now(), Err(TlsError::Crypto));
        source.mode.store(0, Ordering::SeqCst);
        assert_eq!(second.check(), Err(TlsError::Crypto));
        assert_eq!(source.calls.load(Ordering::SeqCst), calls + 1);
        assert_eq!(format!("{second:?}"), "TlsSessionClock(<redacted>)");
    }
}
