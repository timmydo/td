//! Immutable observations, explicit time selection, and bounded retention.
use crate::budget::{Budget, Error as BudgetError, MemoryVec};
use std::sync::Arc;
pub const SAMPLES: usize = 240;
pub const WINDOW_NS: u64 = 120_000_000_000;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Interval {
    HalfSecond,
    Second,
    TwoSeconds,
    FiveSeconds,
}
impl Interval {
    pub fn nanoseconds(self) -> u64 {
        match self {
            Self::HalfSecond => 500_000_000,
            Self::Second => 1_000_000_000,
            Self::TwoSeconds => 2_000_000_000,
            Self::FiveSeconds => 5_000_000_000,
        }
    }
    pub fn samples(self) -> usize {
        (WINDOW_NS / self.nanoseconds()) as usize
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct SampleId(u64);
#[derive(Debug)]
pub struct Sample<T> {
    pub id: SampleId,
    pub time_ns: u64,
    pub skipped: u64,
    pub value: T,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Time,
    Exhausted,
    Budget(BudgetError),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Time => f.write_str("observation time must increase"),
            Self::Exhausted => f.write_str("observation identifiers exhausted"),
            Self::Budget(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for Error {}
#[derive(Debug)]
pub struct History<T> {
    samples: MemoryVec<Sample<T>>,
    budget: Arc<Budget>,
    interval: Interval,
    pinned: Option<SampleId>,
    next: u64,
    last_time: Option<u64>,
    pressure: bool,
}
impl<T> History<T> {
    /// Retained payload allocations must use the same budget as this index.
    pub fn new(budget: &Arc<Budget>, interval: Interval) -> Result<Self, Error> {
        Ok(Self {
            samples: MemoryVec::new(budget, SAMPLES).map_err(Error::Budget)?,
            budget: Arc::clone(budget),
            interval,
            pinned: None,
            next: 1,
            last_time: None,
            pressure: false,
        })
    }
    pub fn samples(&self) -> &[Sample<T>] {
        &self.samples
    }
    pub fn pinned(&self) -> Option<SampleId> {
        self.pinned
    }
    pub fn pressure(&self) -> bool {
        self.pressure
    }
    pub fn interval(&self) -> Interval {
        self.interval
    }
    pub fn set_interval(&mut self, interval: Interval) {
        self.interval = interval;
        self.trim(self.last_time.unwrap_or(0), 0);
    }
    pub fn inspect(&mut self, id: SampleId) -> bool {
        if !self.samples.iter().any(|sample| sample.id == id) {
            return false;
        }
        self.pinned = Some(id);
        true
    }
    pub fn live(&mut self) {
        self.pinned = None;
        self.pressure = false;
        self.trim(self.last_time.unwrap_or(0), 0);
    }
    pub fn selected(&self) -> Option<&Sample<T>> {
        if let Some(id) = self.pinned {
            self.samples.iter().find(|sample| sample.id == id)
        } else {
            self.samples.last()
        }
    }
    pub fn at(&self, time_ns: u64) -> Option<&Sample<T>> {
        self.samples
            .iter()
            .min_by_key(|sample| (sample.time_ns.abs_diff(time_ns), sample.time_ns))
    }
    fn remove(&mut self, index: usize) -> bool {
        self.samples.remove(index).is_some()
    }
    fn evict_oldest(&mut self) -> bool {
        let index = self
            .samples
            .iter()
            .position(|sample| Some(sample.id) != self.pinned);
        index.is_some_and(|index| self.remove(index))
    }
    fn trim(&mut self, time_ns: u64, incoming: usize) {
        let mut index = 0;
        while let Some(sample) = self.samples.get(index) {
            if Some(sample.id) != self.pinned && time_ns.saturating_sub(sample.time_ns) >= WINDOW_NS
            {
                self.remove(index);
            } else {
                index += 1;
            }
        }
        while self.samples.len().saturating_add(incoming) > self.interval.samples() {
            if !self.evict_oldest() {
                break;
            }
        }
    }
    /// Reclaim at most one observation per UI tick before retrying allocation.
    pub fn reclaim_one(&mut self) -> bool {
        let reclaimed = self.evict_oldest();
        self.pressure = !reclaimed;
        reclaimed
    }
    /// Make space before growth. A pinned sample is never removed or moved.
    /// Other budget users may allocate concurrently, so callers still handle
    /// a refused allocation and retry on a later collection tick.
    pub fn make_room(&mut self, bytes: usize) -> bool {
        while self.budget.maximum().saturating_sub(self.budget.used()) < bytes {
            if !self.evict_oldest() {
                self.pressure = true;
                return false;
            }
        }
        self.pressure = false;
        true
    }
    pub fn admit(&mut self, time_ns: u64, skipped: u64, value: T) -> Result<SampleId, (Error, T)> {
        if self.last_time.is_some_and(|last| time_ns <= last) {
            return Err((Error::Time, value));
        }
        let Some(next) = self.next.checked_add(1) else {
            return Err((Error::Exhausted, value));
        };
        self.trim(time_ns, 1);
        let id = SampleId(self.next);
        let sample = Sample {
            id,
            time_ns,
            skipped,
            value,
        };
        if let Err(sample) = self.samples.push(sample) {
            return Err((Error::Budget(BudgetError::Limit), sample.value));
        }
        self.next = next;
        self.last_time = Some(time_ns);
        self.pressure = false;
        Ok(id)
    }
    /// The old inspected point is separate from the current rolling duration.
    pub fn retained_duration_ns(&self) -> u64 {
        let Some(last) = self.last_time else { return 0 };
        self.samples
            .iter()
            .find(|sample| last.saturating_sub(sample.time_ns) < WINDOW_NS)
            .map(|first| last.saturating_sub(first.time_ns))
            .unwrap_or(0)
    }
}
