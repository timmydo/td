#![allow(clippy::unwrap_used, clippy::panic)]
use super::*;
use std::sync::{atomic::AtomicUsize, Barrier};

// Serialize unwrap-based fixtures against the shared process ID CAS;
// the contention fixture deliberately accepts transient Busy.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_CONSTRUCTION_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn current_retired_and_candidate_share_two_slots_before_construction() {
    let _serial = lock();
    let mut set = GenerationSet::at_startup();
    assert_eq!(set.available(), 2);
    assert!(set.current().is_none());
    let abandoned = set.reserve().unwrap();
    assert_eq!(set.available(), 1);
    drop(abandoned);
    assert_eq!(set.available(), 2);
    let first = set.prepare(|| Ok(Box::new(7))).unwrap();
    assert_eq!(set.available(), 1);
    assert!(set.current().is_none());
    assert!(set.publish(first).unwrap().into_lease().is_none());
    let held = set.current().unwrap();
    let cloned = held.clone();
    let old_id = held.id();
    assert_eq!(*held.value(), 7);
    let next = set.prepare(|| Ok(Box::new(8))).unwrap();
    assert_eq!(set.available(), 0);
    assert_eq!(
        set.prepare(|| panic!("third payload loader ran"))
            .unwrap_err(),
        Error::Busy
    );
    let retired = set.publish(next).unwrap().into_lease().unwrap();
    assert_eq!(retired.id(), old_id);
    assert_eq!(*set.current().unwrap().value(), 8);
    assert_ne!(set.current().unwrap().id(), old_id);
    drop(retired);
    drop(held);
    assert_eq!(set.available(), 0);
    drop(cloned);
    assert_eq!(set.available(), 1);
    for number in 9..41 {
        let before = set.current().unwrap().id();
        let candidate = set.prepare(|| Ok(Box::new(number))).unwrap();
        let retired = set.publish(candidate).unwrap().into_lease().unwrap();
        assert_eq!(*set.current().unwrap().value(), number);
        assert!(set.current().unwrap().id().get() > before.get());
        assert_eq!(set.available(), 0);
        drop(retired);
        assert_eq!(set.available(), 1);
    }
}

#[test]
fn failed_stale_and_foreign_candidates_preserve_active_state_and_owners() {
    let _serial = lock();
    let mut set = GenerationSet::at_startup();
    let reserved = set.reserve().unwrap();
    let second = set.prepare(|| Ok(Box::new("secret second"))).unwrap();
    assert!(set.publish(second).unwrap().into_lease().is_none());
    let active = set.current().unwrap().id();
    let first = reserved.construct(|| Ok(Box::new("secret first"))).unwrap();
    let refusal = set.publish(first).unwrap_err();
    assert_eq!(refusal.error(), Error::Conflict);
    assert_eq!(set.current().unwrap().id(), active);
    assert_eq!(set.available(), 0);
    assert!(!format!("{refusal:?}").contains("secret"));
    drop(refusal);
    assert_eq!(set.available(), 1);
    assert_eq!(set.prepare(|| Err(Error::Tls)).unwrap_err(), Error::Tls);
    assert_eq!(set.available(), 1);
    assert_eq!(set.current().unwrap().id(), active);
    for counter in [AtomicU64::new(0), AtomicU64::new(u64::MAX)] {
        assert_eq!(
            set.reserve_using(&counter)
                .and_then(|reserved| reserved.construct(|| panic!("exhausted loader ran")))
                .unwrap_err(),
            Error::Capacity
        );
        assert_eq!(set.available(), 1);
    }
    let candidate = set.prepare(|| Ok(Box::new("secret replacement"))).unwrap();
    let mut other = GenerationSet::at_startup();
    let refusal = other.publish(candidate).unwrap_err();
    assert_eq!(refusal.error(), Error::Forbidden);
    assert_eq!(other.available(), 2);
    assert!(other.current().is_none());
    assert_eq!(set.available(), 0);
    let retired = set.publish(refusal.into_prepared()).unwrap();
    assert!(!format!("{retired:?}").contains("secret"));
    let retired = retired.into_lease().unwrap();
    assert_eq!(retired.id(), active);
    assert!(!format!("{:?}", set.current().unwrap()).contains("secret"));
    drop(retired);
    let replacement_id = set.current().unwrap().id();
    drop(set);
    let candidate = other.prepare(|| Ok(Box::new("new domain"))).unwrap();
    drop(other.publish(candidate).unwrap());
    assert!(other.current().unwrap().id().get() > replacement_id.get());
}

#[test]
fn final_worker_release_destroys_payload_before_returning_capacity() {
    let _serial = lock();
    struct Payload {
        budget: std::sync::Weak<Budget>,
        dropped: Arc<AtomicUsize>,
    }
    impl Drop for Payload {
        fn drop(&mut self) {
            let budget = self.budget.upgrade().unwrap();
            assert_eq!(budget.occupied.load(Ordering::Acquire), 1);
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
    let mut set = GenerationSet::at_startup();
    let budget = Arc::clone(&set.budget);
    let weak = Arc::downgrade(&budget);
    let dropped = Arc::new(AtomicUsize::new(0));
    let reserved = set.reserve().unwrap();
    assert_eq!(set.available(), 1);
    let worker_weak = weak.clone();
    let worker_dropped = dropped.clone();
    let candidate = std::thread::spawn(move || {
        reserved.construct(|| {
            Ok(Box::new(Payload {
                budget: worker_weak,
                dropped: worker_dropped,
            }))
        })
    })
    .join()
    .unwrap()
    .unwrap();
    drop(set.publish(candidate).unwrap());
    let held = set.current().unwrap();
    let barrier = Barrier::new(9);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let lease = held.clone();
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                drop(lease);
            });
        }
        drop(held);
        drop(set);
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        assert_eq!(budget.occupied.load(Ordering::Acquire), 1);
        barrier.wait();
    });
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_eq!(budget.occupied.load(Ordering::Acquire), 0);
    drop(budget);
    assert!(weak.upgrade().is_none());
    // The payload allocation must be separate: it is freed before the permit.
    assert!(std::mem::size_of::<Entry<[u8; 1024 * 1024]>>() <= 64);
    let mut large = GenerationSet::<[u8; 1024 * 1024]>::at_startup();
    let reserved = large.reserve().unwrap();
    let prepared = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || {
            reserved.construct(|| {
                vec![0u8; 1024 * 1024]
                    .into_boxed_slice()
                    .try_into()
                    .map_err(|_| Error::Invalid)
            })
        })
        .unwrap()
        .join()
        .unwrap()
        .unwrap();
    drop(large.publish(prepared).unwrap());
    assert_eq!(large.current().unwrap().value().len(), 1024 * 1024);
}

#[test]
fn racing_preparation_and_release_never_construct_a_third_live_payload() {
    let _serial = lock();
    struct Live(Arc<AtomicUsize>);
    impl Drop for Live {
        fn drop(&mut self) {
            assert!(self.0.fetch_sub(1, Ordering::SeqCst) > 0);
        }
    }
    let set = GenerationSet::at_startup();
    let live = Arc::new(AtomicUsize::new(0));
    let successes = AtomicUsize::new(0);
    let start = Barrier::new(8);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let set = &set;
            let live = &live;
            let successes = &successes;
            let start = &start;
            scope.spawn(move || {
                start.wait();
                for step in 0..2000 {
                    match set.prepare(|| {
                        assert!(live.fetch_add(1, Ordering::SeqCst) < 2);
                        Ok(Box::new(Live(Arc::clone(live))))
                    }) {
                        Ok(candidate) => {
                            successes.fetch_add(1, Ordering::Relaxed);
                            if step % 16 == 0 {
                                std::thread::yield_now();
                            }
                            drop(candidate);
                        }
                        Err(Error::Busy) => {}
                        Err(error) => panic!("unexpected construction error: {error:?}"),
                    }
                }
            });
        }
    });
    assert!(successes.load(Ordering::Relaxed) > 0);
    assert_eq!(live.load(Ordering::SeqCst), 0);
    assert_eq!(set.available(), 2);
    let make = || {
        live.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Live(live.clone())))
    };
    let first = set.prepare(make).unwrap();
    let second = set.prepare(make).unwrap();
    assert_eq!(set.available(), 0);
    assert_eq!(
        set.prepare(|| panic!("saturated loader ran")).unwrap_err(),
        Error::Busy
    );
    drop((first, second));
    assert_eq!(live.load(Ordering::SeqCst), 0);
    assert_eq!(set.available(), 2);
}
