//! Shared injected time with serialized callbacks and unwind retirement.
use crate::TlsError;
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

/// Caller-supplied UTC source. Callbacks must perform bounded, nonblocking work
/// and must not reenter their ClockHandle.
/// Return seconds since Unix epoch, or None when trustworthy time is unavailable.
pub trait UtcClock: Send + Sync {
    fn now(&self) -> Option<u64>;
}

/// Own one clock source and share this handle across configurations. A callback
/// unwind retires the source for every holder; missing time alone does not.
pub struct ClockHandle {
    source: Mutex<Option<Box<dyn UtcClock>>>,
    // Set under the lock before a failing callback returns, so a health check
    // need not wait behind another caller's callback.
    retired: AtomicBool,
}

impl ClockHandle {
    /// Cold construction; does not call the source or read the system clock.
    pub fn new(source: impl UtcClock + 'static) -> Self {
        Self {
            source: Mutex::new(Some(Box::new(source))),
            retired: AtomicBool::new(false),
        }
    }

    /// Whether the source is unretired and unpoisoned, without waiting for a
    /// callback in progress: one still running that has not yet retired the
    /// source is ordered after this check.
    pub(super) fn health(&self) -> Result<(), TlsError> {
        if self.retired.load(Ordering::Acquire) || self.source.is_poisoned() {
            Err(TlsError::Crypto)
        } else {
            Ok(())
        }
    }

    /// Query the shared source. None maps to Clock; a caught unwind or poisoned
    /// source maps to Crypto and permanently refuses further callbacks.
    pub fn now(&self) -> Result<u64, TlsError> {
        let retire = || {
            self.retired.store(true, Ordering::Release);
            TlsError::Crypto
        };
        let mut slot = self.source.lock().map_err(|_| retire())?;
        let source = slot.take().ok_or(TlsError::Crypto)?;
        // This boundary consumes the source; an unwind cannot restore it or
        // expose its possibly changed state to another serialized callback.
        let (source, now) = catch_unwind(AssertUnwindSafe(move || {
            let now = source.now();
            (source, now)
        }))
        .map_err(|_| retire())?;
        *slot = Some(source);
        now.ok_or(TlsError::Clock)
    }
}
impl std::fmt::Debug for ClockHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClockHandle(<redacted>)")
    }
}

pub(super) struct BackendClock(pub(super) Arc<ClockHandle>);
impl std::fmt::Debug for BackendClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TlsClock(<redacted>)")
    }
}
impl rustls::time_provider::TimeProvider for BackendClock {
    fn current_time(&self) -> Option<rustls::pki_types::UnixTime> {
        self.0.now().ok().map(|seconds| {
            rustls::pki_types::UnixTime::since_unix_epoch(std::time::Duration::from_secs(seconds))
        })
    }
}

#[cfg(test)]
#[path = "tls_clock_tests.rs"]
mod tests;
