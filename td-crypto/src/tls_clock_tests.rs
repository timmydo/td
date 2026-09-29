#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use rustls::time_provider::TimeProvider;
use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Barrier,
};

struct Source(Arc<AtomicU64>);
impl UtcClock for Source {
    fn now(&self) -> Option<u64> {
        match self.0.load(Ordering::SeqCst) {
            0 => None,
            u64::MAX => panic!("synthetic clock failure"),
            seconds => Some(seconds),
        }
    }
}
#[test]
fn injected_clock_missing_time_and_shared_retirement() {
    fn shareable<T: Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe>() {}
    shareable::<ClockHandle>();
    let value = Arc::new(AtomicU64::new(1_800_000_000));
    let clock = Arc::new(ClockHandle::new(Source(value.clone())));
    let backend_a = BackendClock(clock.clone());
    let backend_b = BackendClock(clock.clone());
    assert_eq!(clock.now(), Ok(1_800_000_000));
    assert_eq!(backend_a.current_time().unwrap().as_secs(), 1_800_000_000);
    assert_eq!(format!("{clock:?}"), "ClockHandle(<redacted>)");
    assert_eq!(format!("{backend_a:?}"), "TlsClock(<redacted>)");
    value.store(0, Ordering::SeqCst);
    assert_eq!(clock.now(), Err(TlsError::Clock));
    assert!(backend_a.current_time().is_none());
    value.store(1, Ordering::SeqCst);
    assert_eq!(backend_b.current_time().unwrap().as_secs(), 1);
    value.store(u64::MAX, Ordering::SeqCst);
    assert!(backend_a.current_time().is_none());
    value.store(1_800_000_000, Ordering::SeqCst);
    assert_eq!(clock.now(), Err(TlsError::Crypto));
    assert!(backend_b.current_time().is_none());
    assert!(clock.source.lock().unwrap().is_none());
    struct Maximum;
    impl UtcClock for Maximum {
        fn now(&self) -> Option<u64> {
            Some(u64::MAX)
        }
    }
    assert_eq!(
        BackendClock(Arc::new(ClockHandle::new(Maximum)))
            .current_time()
            .unwrap()
            .as_secs(),
        u64::MAX
    );
}
#[test]
fn injected_clock_serializes_and_drops_panicking_source() {
    struct Failing {
        calls: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
        entered: Arc<Barrier>,
        release: Arc<Barrier>,
    }
    impl UtcClock for Failing {
        fn now(&self) -> Option<u64> {
            assert_eq!(self.calls.fetch_add(1, Ordering::SeqCst), 0);
            self.entered.wait();
            self.release.wait();
            panic!("synthetic shared callback failure")
        }
    }
    impl Drop for Failing {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let clock = Arc::new(ClockHandle::new(Failing {
        calls: calls.clone(),
        drops: drops.clone(),
        entered: entered.clone(),
        release: release.clone(),
    }));
    let first = clock.clone();
    let first = std::thread::spawn(move || first.now());
    entered.wait();
    let second = clock.clone();
    let second = std::thread::spawn(move || second.now());
    release.wait();
    assert_eq!(first.join().unwrap(), Err(TlsError::Crypto));
    assert_eq!(second.join().unwrap(), Err(TlsError::Crypto));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(clock.now(), Err(TlsError::Crypto));

    let value = Arc::new(AtomicU64::new(7));
    let poisoned = ClockHandle::new(Source(value));
    assert!(std::panic::catch_unwind(|| {
        let _guard = poisoned.source.lock().unwrap();
        panic!("synthetic poisoned boundary")
    })
    .is_err());
    assert_eq!(poisoned.now(), Err(TlsError::Crypto));
}
