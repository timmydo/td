//! Caller-owned fixed queues and process-local checked slot reuse tokens.
//! Synchronization and completion-credit reservation belong to the scheduler.
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};

/// This counter is never reset, including when a pool is dropped/reconstructed.
/// Relaxed ordering supplies uniqueness only; it does not synchronize slot data.
static NEXT_TICKET: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    OccupiedStorage,
    TooManySlots,
    Full,
    GenerationExhausted,
    Contended,
    StaleSlot,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::OccupiedStorage => "backing storage is occupied",
            Self::TooManySlots => "slot index exceeds its representation",
            Self::Full => "slot pool is full",
            Self::GenerationExhausted => "slot generation space exhausted",
            Self::Contended => "slot generation issuer is busy",
            Self::StaleSlot => "stale or foreign slot token",
        })
    }
}

impl std::error::Error for Error {}

/// The actual Option<T> layout, not just T, must fit the caller's queue ledger.
/// Values must themselves be bounded; this container cannot constrain T's heap.
/// Dropping the queue drops remaining values in FIFO order and clears their cells.
/// There is no implicit locking, waiting, allocation, or capacity growth.
/// Runtime payload destructors must also obey the panic/allocation/work bounds.
pub struct FixedQueue<'a, T> {
    entries: &'a mut [Option<T>],
    head: usize,
    tail: usize,
    count: usize,
}

impl<'a, T> FixedQueue<'a, T> {
    /// Refuses occupied storage without changing or dropping its values.
    pub fn new(entries: &'a mut [Option<T>]) -> Result<Self, Error> {
        if entries.iter().any(Option::is_some) {
            return Err(Error::OccupiedStorage);
        }
        Ok(Self {
            entries,
            head: 0,
            tail: 0,
            count: 0,
        })
    }

    pub fn capacity(&self) -> usize {
        self.entries.len()
    }
    pub fn len(&self) -> usize {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Saturation returns ownership of the value without changing the queue.
    pub fn push(&mut self, value: T) -> Result<(), T> {
        if self.count >= self.entries.len() {
            return Err(value);
        }
        let Some(after_tail) = self.tail.checked_add(1) else {
            return Err(value);
        };
        let Some(count) = self.count.checked_add(1) else {
            return Err(value);
        };
        let capacity = self.entries.len();
        let Some(entry) = self.entries.get_mut(self.tail) else {
            return Err(value);
        };
        if entry.is_some() {
            return Err(value);
        }
        *entry = Some(value);
        self.tail = if after_tail == capacity {
            0
        } else {
            after_tail
        };
        self.count = count;
        Ok(())
    }

    pub fn pop(&mut self) -> Option<T> {
        if self.count == 0 {
            return None;
        }
        let after_head = self.head.checked_add(1)?;
        let count = self.count.checked_sub(1)?;
        let value = self.entries.get_mut(self.head)?.take()?;
        self.head = if after_head == self.entries.len() {
            0
        } else {
            after_head
        };
        self.count = count;
        Some(value)
    }
}

impl<T> Drop for FixedQueue<'_, T> {
    fn drop(&mut self) {
        while self.pop().is_some() {}
    }
}

/// Only a live pool can mint a token. Neither the index nor the generation alone
/// establishes ownership. Tokens are boot/process-local, never persisted IDs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct SlotId {
    generation: NonZeroU64,
    index: u32,
}

#[derive(Debug, Default)]
pub struct SlotState {
    generation: Option<NonZeroU64>,
}

impl SlotState {
    /// Use `[const { SlotState::EMPTY }; N]` or `array::from_fn` at startup.
    pub const EMPTY: Self = Self { generation: None };
}

/// Bookkeeping over separately budgeted slot payloads. This does not lock or
/// borrow those payloads: the scheduler must use RESOURCES.md's lock ordering
/// and token revalidation before accessing or releasing a slot.
/// Dropping a pool invalidates every outstanding token, without freeing payloads.
pub struct SlotPool<'a> {
    slots: &'a mut [SlotState],
}

impl<'a> SlotPool<'a> {
    pub fn new(slots: &'a mut [SlotState]) -> Result<Self, Error> {
        if u32::try_from(slots.len()).is_err() {
            return Err(Error::TooManySlots);
        }
        if slots.iter().any(|slot| slot.generation.is_some()) {
            return Err(Error::OccupiedStorage);
        }
        Ok(Self { slots })
    }

    /// Bounded scan of caller-specified slots. Exhaustion never wraps a ticket.
    /// Returns Contended if another issuer races this one, without reserving a slot;
    /// the scheduler retries on a later turn, never spins in this operation.
    pub fn acquire(&mut self) -> Result<SlotId, Error> {
        self.acquire_using(|| next_ticket(&NEXT_TICKET))
    }

    fn acquire_using(
        &mut self,
        issue: impl FnOnce() -> Result<NonZeroU64, Error>,
    ) -> Result<SlotId, Error> {
        let (index, slot) = self
            .slots
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| slot.generation.is_none())
            .ok_or(Error::Full)?;
        let index = u32::try_from(index).map_err(|_| Error::TooManySlots)?;
        let generation = issue()?;
        slot.generation = Some(generation);
        Ok(SlotId { generation, index })
    }

    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// A bounded scan, not a reservation or synchronization primitive.
    pub fn available(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.generation.is_none())
            .count()
    }

    /// An initial lookup can locate the payload lock. After taking that lock,
    /// revalidate under the pool lock before payload access. Keep payload
    /// ownership across I/O, but release the pool lock (see RESOURCES.md).
    pub fn resolve(&self, id: SlotId) -> Result<usize, Error> {
        let index = usize::try_from(id.index).map_err(|_| Error::StaleSlot)?;
        let slot = self.slots.get(index).ok_or(Error::StaleSlot)?;
        if slot.generation != Some(id.generation) {
            return Err(Error::StaleSlot);
        }
        Ok(index)
    }

    pub fn contains(&self, id: SlotId) -> bool {
        self.resolve(id).is_ok()
    }

    pub fn release(&mut self, id: SlotId) -> Result<(), Error> {
        let index = self.resolve(id)?;
        let slot = self.slots.get_mut(index).ok_or(Error::StaleSlot)?;
        slot.generation = None;
        Ok(())
    }
}

impl Drop for SlotPool<'_> {
    fn drop(&mut self) {
        for slot in self.slots.iter_mut() {
            slot.generation = None;
        }
    }
}

fn next_ticket(counter: &AtomicU64) -> Result<NonZeroU64, Error> {
    let current = counter.load(Ordering::Relaxed);
    if current == 0 {
        return Err(Error::GenerationExhausted);
    }
    let next = current.checked_add(1).ok_or(Error::GenerationExhausted)?;
    let ticket = counter
        .compare_exchange(current, next, Ordering::Relaxed, Ordering::Relaxed)
        .map_err(|_| Error::Contended)?;
    NonZeroU64::new(ticket).ok_or(Error::GenerationExhausted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::VecDeque;

    fn acquire(pool: &mut SlotPool<'_>) -> Result<SlotId, Error> {
        for _ in 0..1000 {
            match pool.acquire() {
                Err(Error::Contended) => std::thread::yield_now(),
                result => return result,
            }
        }
        Err(Error::Contended)
    }

    #[test]
    fn queue_wraparound_and_saturation_match_fifo() -> Result<(), Error> {
        for capacity in 0..17 {
            let mut storage = vec![None; capacity];
            let mut queue = FixedQueue::new(&mut storage)?;
            let mut oracle = VecDeque::new();
            let mut random = 123_u32;
            for step in 0..4096 {
                random = random.wrapping_mul(1664525).wrapping_add(1013904223);
                if (random >> 16) & 1 == 0 {
                    let result = queue.push(step);
                    if oracle.len() == capacity {
                        assert_eq!(result, Err(step));
                    } else {
                        assert_eq!(result, Ok(()));
                        oracle.push_back(step);
                    }
                } else {
                    assert_eq!(queue.pop(), oracle.pop_front());
                }
                assert_eq!(queue.len(), oracle.len());
                assert_eq!(queue.is_empty(), oracle.is_empty());
                assert_eq!(queue.capacity(), capacity);
            }
            while !oracle.is_empty() {
                assert_eq!(queue.pop(), oracle.pop_front());
            }
            assert_eq!(queue.pop(), None);
        }
        Ok(())
    }

    struct Tracked<'a>(&'a Cell<usize>);
    impl Drop for Tracked<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn queue_refusal_and_drop_preserve_value_ownership() -> Result<(), Error> {
        let dropped = Cell::new(0);
        let mut storage = [None];
        {
            let mut queue = FixedQueue::new(&mut storage)?;
            assert!(queue.push(Tracked(&dropped)).is_ok());
            let rejected = queue.push(Tracked(&dropped));
            assert!(rejected.is_err());
            assert_eq!(dropped.get(), 0);
            drop(rejected);
            assert_eq!(dropped.get(), 1);
        }
        assert_eq!(dropped.get(), 2);
        assert!(storage.iter().all(Option::is_none));
        let mut occupied = [Some(Tracked(&dropped))];
        assert!(matches!(
            FixedQueue::new(&mut occupied),
            Err(Error::OccupiedStorage)
        ));
        assert_eq!(dropped.get(), 2);
        assert!(occupied.iter().all(Option::is_some));
        Ok(())
    }

    #[test]
    fn stale_foreign_and_reconstructed_pool_tokens_are_rejected() -> Result<(), Error> {
        let mut storage = [SlotState::default()];
        let mut other_storage = [SlotState::default()];
        let mut other = SlotPool::new(&mut other_storage)?;
        let foreign = acquire(&mut other)?;
        let old;
        {
            let mut pool = SlotPool::new(&mut storage)?;
            old = acquire(&mut pool)?;
            assert!(pool.contains(old));
            assert!(!pool.contains(foreign));
            assert_eq!(pool.release(foreign), Err(Error::StaleSlot));
            assert_eq!(pool.acquire(), Err(Error::Full));
            pool.release(old)?;
            let reused = acquire(&mut pool)?;
            assert_eq!(pool.resolve(reused)?, 0);
            assert_eq!(pool.resolve(old), Err(Error::StaleSlot));
            assert_eq!(reused.index, old.index);
            assert_ne!(reused.generation, old.generation);
            assert_eq!(pool.release(old), Err(Error::StaleSlot));
            assert!(pool.contains(reused));
        }
        let mut rebuilt = SlotPool::new(&mut storage)?;
        let current = acquire(&mut rebuilt)?;
        assert_ne!(current, old);
        assert!(!rebuilt.contains(old));
        assert!(other.contains(foreign));
        rebuilt.release(current)?;
        assert_eq!(rebuilt.release(current), Err(Error::StaleSlot));
        Ok(())
    }

    #[test]
    fn generation_exhaustion_never_wraps_or_changes_counter() -> Result<(), Error> {
        let local = AtomicU64::new(u64::MAX - 1);
        assert_eq!(next_ticket(&local)?.get(), u64::MAX - 1);
        assert_eq!(next_ticket(&local), Err(Error::GenerationExhausted));
        assert_eq!(next_ticket(&local), Err(Error::GenerationExhausted));
        assert_eq!(local.load(Ordering::Relaxed), u64::MAX);
        assert_eq!(
            next_ticket(&AtomicU64::new(0)),
            Err(Error::GenerationExhausted)
        );
        Ok(())
    }

    #[test]
    fn concurrent_issuers_never_duplicate_a_generation() -> Result<(), Error> {
        let counter = AtomicU64::new(1);
        let results = std::thread::scope(|scope| {
            let mut workers = Vec::new();
            for _ in 0..8 {
                let counter = &counter;
                workers.push(scope.spawn(move || {
                    let mut issued = Vec::new();
                    for _ in 0..100000 {
                        match next_ticket(counter) {
                            Ok(ticket) => issued.push(ticket.get()),
                            Err(Error::Contended) => continue,
                            Err(error) => return Err(error),
                        }
                        if issued.len() == 500 {
                            return Ok(issued);
                        }
                    }
                    Err(Error::Contended)
                }));
            }
            let mut all = Vec::new();
            for worker in workers {
                match worker.join() {
                    Ok(result) => all.extend(result?),
                    Err(payload) => std::panic::resume_unwind(payload),
                }
            }
            Ok::<_, Error>(all)
        })?;
        let mut sorted = results;
        sorted.sort_unstable();
        assert_eq!(sorted, (1..=4000).collect::<Vec<_>>());
        Ok(())
    }

    #[test]
    fn empty_and_occupied_slot_storage_refuse_without_changes() -> Result<(), Error> {
        let mut empty = [];
        assert_eq!(SlotPool::new(&mut empty)?.acquire(), Err(Error::Full));
        let generation = Some(next_ticket(&AtomicU64::new(1))?);
        let mut storage = [SlotState { generation }];
        assert!(matches!(
            SlotPool::new(&mut storage),
            Err(Error::OccupiedStorage)
        ));
        assert!(storage.iter().all(|slot| slot.generation == generation));
        Ok(())
    }

    #[test]
    fn every_small_ring_fills_and_refuses_at_every_rotation() -> Result<(), Error> {
        for capacity in 0..17 {
            for rotation in 0..capacity.max(1) {
                let mut storage = vec![None; capacity];
                let mut queue = FixedQueue::new(&mut storage)?;
                for _ in 0..rotation {
                    assert_eq!(queue.push(999), Ok(()));
                    assert_eq!(queue.pop(), Some(999));
                }
                for value in 0..capacity {
                    assert_eq!(queue.push(value), Ok(()));
                }
                assert_eq!(queue.len(), capacity);
                assert_eq!(queue.push(999), Err(999));
                for value in 0..capacity {
                    assert_eq!(queue.pop(), Some(value));
                }
                assert_eq!(queue.pop(), None);
            }
        }
        Ok(())
    }

    #[test]
    fn wrapped_queue_drops_in_fifo_order() -> Result<(), Error> {
        use std::cell::RefCell;
        struct Ordered<'a>(u8, &'a RefCell<Vec<u8>>);
        impl Drop for Ordered<'_> {
            fn drop(&mut self) {
                self.1.borrow_mut().push(self.0);
            }
        }
        let order = RefCell::new(Vec::new());
        let mut storage = [None, None, None];
        {
            let mut queue = FixedQueue::new(&mut storage)?;
            for i in 0..3 {
                assert!(queue.push(Ordered(i, &order)).is_ok());
            }
            drop(queue.pop());
            assert!(queue.push(Ordered(3, &order)).is_ok());
        }
        assert_eq!(*order.borrow(), [0, 1, 2, 3]);
        assert!(storage.iter().all(Option::is_none));
        Ok(())
    }

    #[test]
    fn failed_issuance_and_foreign_out_of_range_tokens_leave_slots_unchanged() -> Result<(), Error>
    {
        let mut storage = [const { SlotState::EMPTY }; 1];
        let mut pool = SlotPool::new(&mut storage)?;
        assert_eq!(pool.capacity(), 1);
        assert_eq!(pool.available(), 1);
        assert_eq!(
            pool.acquire_using(|| Err(Error::Contended)),
            Err(Error::Contended)
        );
        let exhausted = AtomicU64::new(u64::MAX);
        assert_eq!(
            pool.acquire_using(|| next_ticket(&exhausted)),
            Err(Error::GenerationExhausted)
        );
        assert_eq!(pool.available(), 1);
        let mut foreign_storage = [const { SlotState::EMPTY }; 2];
        let mut foreign = SlotPool::new(&mut foreign_storage)?;
        acquire(&mut foreign)?;
        let outside = acquire(&mut foreign)?;
        assert_eq!(outside.index, 1);
        assert!(!pool.contains(outside));
        assert_eq!(pool.resolve(outside), Err(Error::StaleSlot));
        assert_eq!(pool.release(outside), Err(Error::StaleSlot));
        assert_eq!(pool.available(), 1);
        let token = acquire(&mut pool)?;
        assert_eq!(pool.available(), 0);
        assert_eq!(
            pool.acquire_using(|| Err(Error::Contended)),
            Err(Error::Full)
        );
        assert!(pool.contains(token));
        pool.release(token)?;
        assert_eq!(pool.available(), 1);
        Ok(())
    }

    #[test]
    fn token_layout_leaves_room_in_32_byte_queue_cells() {
        #[allow(dead_code)]
        struct Job {
            slot: SlotId,
            kind_and_args: [u8; 16],
        }
        assert!(std::mem::size_of::<SlotState>() <= 8);
        assert!(std::mem::size_of::<SlotId>() <= 16);
        assert!(std::mem::size_of::<Option<Job>>() <= 32);
    }
}
