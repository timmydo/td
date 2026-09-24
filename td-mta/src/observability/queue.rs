//! Fixed diagnostic buffering. Runtime synchronization and sink I/O belong to M19.
use super::Event;
use crate::ownership::{self, FixedQueue};

pub const MAX_EVENTS: usize = 384;
pub const QUEUE_BYTES: usize = 96 * 1024;

/// A boot-local counter. At the ceiling its value is a lower bound.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Counter {
    value: u64,
}
impl Counter {
    pub const ZERO: Self = Self { value: 0 };
    pub const fn from_value(value: u64) -> Self {
        Self { value }
    }
    pub fn add(&mut self, amount: u64) {
        self.value = self.value.saturating_add(amount);
    }
    pub const fn value(self) -> u64 {
        self.value
    }
    pub const fn saturated(self) -> bool {
        self.value == u64::MAX
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LossSnapshot {
    pub dropped: Counter,
    pub write_failures: Counter,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Capacity,
    Storage(ownership::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Capacity => f.write_str("invalid log queue capacity"),
            Self::Storage(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Capacity => None,
            Self::Storage(e) => Some(e),
        }
    }
}

/// The caller supplies cells from the log reservation and owns exclusive access.
/// Full queues suppress events immediately; they never wait for sink progress.
pub struct EventQueue<'a> {
    queue: FixedQueue<'a, Event>,
    losses: LossSnapshot,
}
impl<'a> EventQueue<'a> {
    pub fn new(cells: &'a mut [Option<Event>]) -> Result<Self, Error> {
        let bytes = cells
            .len()
            .checked_mul(std::mem::size_of::<Option<Event>>())
            .ok_or(Error::Capacity)?;
        if cells.is_empty()
            || cells.len() > MAX_EVENTS
            || bytes > QUEUE_BYTES
            || std::mem::size_of::<Option<Event>>() > 256
        {
            return Err(Error::Capacity);
        }
        Ok(Self {
            queue: FixedQueue::new(cells).map_err(Error::Storage)?,
            losses: LossSnapshot::default(),
        })
    }
    pub fn capacity(&self) -> usize {
        self.queue.capacity()
    }
    pub fn len(&self) -> usize {
        self.queue.len()
    }
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
    /// False means this event was suppressed and the loss counter was increased.
    pub fn try_emit(&mut self, event: Event) -> bool {
        if self.queue.push(event).is_ok() {
            true
        } else {
            self.losses.dropped.add(1);
            false
        }
    }
    /// Ownership moves to the one sink, which retains it through partial writes.
    pub fn pop(&mut self) -> Option<Event> {
        self.queue.pop()
    }
    /// Count sink failures independently of queue saturation, without enqueueing.
    pub fn note_write_failure(&mut self) {
        self.losses.write_failures.add(1);
    }
    /// Count events already popped that the sink definitively abandoned. Do not
    /// count a retryable write error as loss until the event is discarded.
    pub fn note_discarded(&mut self, count: u64) {
        self.losses.dropped.add(count);
    }
    /// Drain queued events and count each discard before releasing their cells.
    /// A popped in-flight event must be counted separately by its sink owner.
    pub fn discard_remaining(&mut self) {
        while self.queue.pop().is_some() {
            self.losses.dropped.add(1);
        }
    }
    /// Cumulative since this boot, never reset on attempted loss reporting.
    pub fn losses(&self) -> LossSnapshot {
        self.losses
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ids::BootId,
        observability::{Context, Kind},
    };
    fn event(bytes: u64) -> Event {
        Event {
            context: Context {
                boot: BootId::from_bytes([1; 16]),
                utc_ms: None,
                config_generation: 0,
                connection: None,
                request: None,
                transaction: None,
                submission: None,
            },
            kind: Kind::MailAccepted { bytes },
        }
    }
    #[test]
    fn floods_are_bounded_keep_fifo_and_report_loss_outside_the_queue() -> Result<(), Error> {
        let mut cells = [None; 2];
        let pointer = cells.as_ptr();
        let mut q = EventQueue::new(&mut cells)?;
        assert!(q.try_emit(event(1)));
        assert!(q.try_emit(event(2)));
        for n in 3..1003 {
            assert!(!q.try_emit(event(n)));
        }
        q.note_write_failure();
        assert_eq!(q.losses().dropped.value(), 1000);
        assert_eq!(q.losses().write_failures.value(), 1);
        assert_eq!(q.capacity(), 2);
        assert_eq!(q.len(), 2);
        assert_eq!(q.pop(), Some(event(1)));
        assert!(q.try_emit(event(1003)));
        assert_eq!(q.pop(), Some(event(2)));
        assert_eq!(q.pop(), Some(event(1003)));
        assert_eq!(q.pop(), None);
        assert!(q.is_empty());
        assert_eq!(q.losses().dropped.value(), 1000);
        assert!(q.try_emit(event(1004)));
        assert!(q.try_emit(event(1005)));
        assert_eq!(q.pop(), Some(event(1004)));
        q.note_write_failure();
        q.note_discarded(1);
        q.discard_remaining();
        q.discard_remaining();
        assert!(q.is_empty());
        assert_eq!(q.losses().dropped.value(), 1002);
        assert_eq!(q.losses().write_failures.value(), 2);
        drop(q);
        assert_eq!(cells.as_ptr(), pointer);
        assert!(cells.iter().all(Option::is_none));
        Ok(())
    }
    #[test]
    fn construction_and_counter_exhaustion_cannot_grow_or_wrap() -> Result<(), Error> {
        assert!(std::mem::size_of::<EventQueue<'_>>() <= 16 * 1024);
        assert_eq!(std::mem::size_of::<Counter>(), 8);
        assert_eq!(Counter::ZERO, Counter::default());
        assert!(Counter::from_value(u64::MAX).saturated());
        assert_eq!(Counter::from_value(7).value(), 7);
        assert!(!Counter::from_value(7).saturated());
        assert!(std::error::Error::source(&Error::Capacity).is_none());
        assert!(std::error::Error::source(&Error::Storage(ownership::Error::Full)).is_some());
        let mut empty = [];
        assert!(matches!(EventQueue::new(&mut empty), Err(Error::Capacity)));
        let mut too_many = [None; MAX_EVENTS + 1];
        assert!(matches!(
            EventQueue::new(&mut too_many),
            Err(Error::Capacity)
        ));
        let mut occupied = [Some(event(1))];
        assert!(matches!(
            EventQueue::new(&mut occupied),
            Err(Error::Storage(ownership::Error::OccupiedStorage))
        ));
        assert_eq!(occupied, [Some(event(1))]);
        let mut cells = [None; MAX_EVENTS];
        assert!(std::mem::size_of_val(&cells) <= QUEUE_BYTES);
        let mut q = EventQueue::new(&mut cells)?;
        q.losses.dropped.add(u64::MAX - 1);
        for _ in 0..MAX_EVENTS {
            assert!(q.try_emit(event(1)));
        }
        for _ in 0..2 {
            assert!(!q.try_emit(event(2)));
        }
        assert_eq!(q.losses().dropped.value(), u64::MAX);
        assert!(q.losses().dropped.saturated());
        let mut c = Counter::default();
        c.add(u64::MAX);
        c.add(0);
        c.add(1);
        assert_eq!(c.value(), u64::MAX);
        assert!(c.saturated());
        drop(q);
        assert!(cells.iter().all(Option::is_none));
        Ok(())
    }
}
