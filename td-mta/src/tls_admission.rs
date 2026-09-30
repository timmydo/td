//! Bounded handshake reservations; connection ownership and policy stay separate.
use crate::ports::Error;
use std::sync::{atomic::AtomicU8, atomic::Ordering, Arc};

const MAX_HANDSHAKES: u8 = 8;

struct State {
    occupied: AtomicU8,
    capacity: u8,
}

/// Construct once at startup using the validated global handshake limit.
/// One cold allocation holds the bitmap; reserve and release never allocate.
/// This capacity counter neither reserves session buffers nor owns connections.
pub struct HandshakePool {
    state: Arc<State>,
}

/// One non-clonable reservation, movable between workers and released on Drop.
/// Keep it through queued, running and idle handshake steps, until completion
/// or terminal teardown. Forgetting it leaks capacity, never over-admits.
///
/// ```compile_fail,E0277
/// use td_mta::tls_admission::HandshakePermit;
/// fn requires_clone<T: Clone>() {}
/// requires_clone::<HandshakePermit>();
/// ```
#[must_use = "retain the reservation until handshake completion or teardown"]
pub struct HandshakePermit {
    state: Arc<State>,
    bit: u8,
}

impl HandshakePool {
    pub fn new(capacity: usize) -> Result<Self, Error> {
        let capacity = u8::try_from(capacity).map_err(|_| Error::Invalid)?;
        if capacity == 0 || capacity > MAX_HANDSHAKES {
            return Err(Error::Invalid);
        }
        let shift = MAX_HANDSHAKES.checked_sub(capacity).ok_or(Error::Invalid)?;
        let allowed = u8::MAX
            .checked_shr(u32::from(shift))
            .ok_or(Error::Invalid)?;
        Ok(Self {
            state: Arc::new(State {
                occupied: AtomicU8::new(!allowed),
                capacity,
            }),
        })
    }

    pub fn capacity(&self) -> usize {
        usize::from(self.state.capacity)
    }

    /// A momentary observation, not a reservation or promise of availability.
    pub fn available(&self) -> usize {
        self.state.occupied.load(Ordering::Relaxed).count_zeros() as usize
    }

    /// Try once without spinning or waiting. Saturation or a racing change
    /// returns Busy without acquiring capacity; retry on a later scheduler turn.
    pub fn reserve(&self) -> Result<HandshakePermit, Error> {
        let occupied = self.state.occupied.load(Ordering::Relaxed);
        let bit = 1u8
            .checked_shl(occupied.trailing_ones())
            .ok_or(Error::Busy)?;
        // The bitmap accounts for capacity only, never publishes slot payloads.
        self.state
            .occupied
            .compare_exchange(
                occupied,
                occupied | bit,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .map_err(|_| Error::Busy)?;
        Ok(HandshakePermit {
            state: Arc::clone(&self.state),
            bit,
        })
    }
}

impl Drop for HandshakePermit {
    fn drop(&mut self) {
        // Only this linear owner can clear its bit; it cannot be reused earlier.
        self.state.occupied.fetch_and(!self.bit, Ordering::Relaxed);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::{sync::Barrier, thread};

    #[test]
    fn capacities_saturate_and_reuse_without_losing_live_reservations() {
        for capacity in 0..=9 {
            let limits = crate::limits::Limits {
                tls_handshakes: capacity,
                memory_budget_bytes: 128 * 1024 * 1024,
                ..Default::default()
            };
            assert_eq!(HandshakePool::new(capacity).is_ok(), limits.plan().is_ok());
        }
        for invalid in [0, 9, 256, usize::MAX] {
            assert!(matches!(HandshakePool::new(invalid), Err(Error::Invalid)));
        }
        for capacity in 1..=8 {
            let pool = HandshakePool::new(capacity).unwrap();
            assert_eq!(pool.capacity(), capacity);
            for _ in 0..32 {
                let mut held = Vec::new();
                for remaining in (0..capacity).rev() {
                    held.push(Some(pool.reserve().unwrap()));
                    assert_eq!(pool.available(), remaining);
                }
                assert!(matches!(pool.reserve(), Err(Error::Busy)));
                for permit in &mut held {
                    let previous_bit = permit.as_ref().unwrap().bit;
                    drop(permit.take());
                    assert_eq!(pool.available(), 1);
                    *permit = Some(pool.reserve().unwrap());
                    assert_eq!(permit.as_ref().unwrap().bit, previous_bit);
                    assert_eq!(pool.available(), 0);
                    assert!(matches!(pool.reserve(), Err(Error::Busy)));
                }
                drop(held);
                assert_eq!(pool.available(), capacity);
            }
        }
    }

    #[test]
    fn concurrent_reservations_and_worker_returns_preserve_the_global_cap() {
        for capacity in 1..=8 {
            let pool = HandshakePool::new(capacity).unwrap();
            let start = Barrier::new(17);
            let mut held = thread::scope(|scope| {
                let mut workers = Vec::new();
                for _ in 0..16 {
                    let pool = &pool;
                    let start = &start;
                    workers.push(scope.spawn(move || {
                        start.wait();
                        pool.reserve()
                    }));
                }
                start.wait();
                let mut held = Vec::new();
                for worker in workers {
                    match worker.join().unwrap() {
                        Ok(permit) => held.push(permit),
                        Err(error) => assert_eq!(error, Error::Busy),
                    }
                }
                held
            });
            assert!(!held.is_empty());
            assert!(held.len() <= capacity);
            assert_eq!(pool.available(), capacity - held.len());
            while held.len() < capacity {
                held.push(pool.reserve().unwrap());
            }
            let mut claimed = 0u8;
            for permit in &held {
                assert_eq!(claimed & permit.bit, 0);
                claimed |= permit.bit;
            }
            assert!(matches!(pool.reserve(), Err(Error::Busy)));
            thread::spawn(move || drop(held)).join().unwrap();
            assert_eq!(pool.available(), capacity);
        }
    }

    #[test]
    fn overlapping_release_and_reservation_never_over_admit_or_leak_capacity() {
        use std::sync::atomic::AtomicUsize;
        for capacity in [1, 2, 8] {
            let pool = HandshakePool::new(capacity).unwrap();
            let start = Barrier::new(8);
            let active = AtomicUsize::new(0);
            let excess = AtomicUsize::new(0);
            let completed = AtomicUsize::new(0);
            thread::scope(|scope| {
                for _ in 0..8 {
                    let (pool, start, active, excess, completed) =
                        (&pool, &start, &active, &excess, &completed);
                    scope.spawn(move || {
                        start.wait();
                        for turn in 0..20_000 {
                            match pool.reserve() {
                                Ok(permit) => {
                                    if active.fetch_add(1, Ordering::SeqCst) >= capacity {
                                        excess.fetch_add(1, Ordering::SeqCst);
                                    }
                                    if turn % 16 == 0 {
                                        thread::yield_now();
                                    }
                                    active.fetch_sub(1, Ordering::SeqCst);
                                    drop(permit);
                                    completed.fetch_add(1, Ordering::SeqCst);
                                }
                                Err(error) => assert_eq!(error, Error::Busy),
                            }
                        }
                    });
                }
            });
            assert!(completed.load(Ordering::SeqCst) > 0);
            assert_eq!(excess.load(Ordering::SeqCst), 0);
            assert_eq!(active.load(Ordering::SeqCst), 0);
            assert_eq!(pool.available(), capacity);
        }
    }

    #[test]
    fn refusal_and_pool_drop_preserve_linear_cleanup() {
        fn refuse(pool: &HandshakePool, error: Error) -> Result<(), Error> {
            let _permit = pool.reserve()?;
            Err(error)
        }
        let pool = HandshakePool::new(1).unwrap();
        for error in [Error::Invalid, Error::Deadline, Error::Tls] {
            assert_eq!(refuse(&pool, error), Err(error));
            assert_eq!(pool.available(), 1);
        }
        let held = pool.reserve().unwrap();
        assert_eq!(refuse(&pool, Error::Tls), Err(Error::Busy));
        assert_eq!(pool.available(), 0);
        let weak = Arc::downgrade(&pool.state);
        drop(pool);
        assert!(weak.upgrade().is_some());
        thread::spawn(move || drop(held)).join().unwrap();
        assert!(weak.upgrade().is_none());
        assert!(std::mem::size_of::<State>() <= 16);
        assert!(std::mem::size_of::<HandshakePool>() <= 16);
        assert!(std::mem::size_of::<HandshakePermit>() <= 16);
    }
}
