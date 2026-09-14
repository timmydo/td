//! Shared accounting for retained model data and in-flight collection buffers.
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
pub const LIMIT: usize = 64 * 1024 * 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Limit,
    Allocation,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Limit => "model memory budget exhausted",
            Self::Allocation => "model allocation failed",
        })
    }
}
impl std::error::Error for Error {}
#[derive(Debug)]
pub struct Budget {
    maximum: usize,
    used: AtomicUsize,
    peak: AtomicUsize,
}
impl Budget {
    pub fn new(maximum: usize) -> Result<Arc<Self>, Error> {
        let base = std::mem::size_of::<Self>() + 2 * std::mem::size_of::<usize>();
        if maximum < base || maximum > LIMIT {
            return Err(Error::Limit);
        }
        // One fixed startup owner; per-buffer growth is reserved fallibly below.
        Ok(Arc::new(Self {
            maximum,
            used: AtomicUsize::new(base),
            peak: AtomicUsize::new(base),
        }))
    }
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }
    pub fn maximum(&self) -> usize {
        self.maximum
    }
    fn reserve(&self, bytes: usize) -> Result<(), Error> {
        let old = self
            .used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
                old.checked_add(bytes).filter(|new| *new <= self.maximum)
            })
            .map_err(|_| Error::Limit)?;
        self.peak.fetch_max(old + bytes, Ordering::Relaxed);
        Ok(())
    }
    pub(crate) fn charge(self: &Arc<Self>, bytes: usize) -> Result<Charge, Error> {
        self.reserve(bytes)?;
        Ok(Charge {
            budget: Arc::clone(self),
            bytes,
        })
    }
}
#[derive(Debug)]
pub(crate) struct Charge {
    budget: Arc<Budget>,
    bytes: usize,
}
impl Charge {
    pub(crate) fn additional(&mut self, bytes: usize) -> Result<(), Error> {
        self.budget.reserve(bytes)?;
        self.bytes += bytes;
        Ok(())
    }
}
impl Drop for Charge {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}
#[derive(Debug)]
pub struct MemoryVec<T> {
    data: Vec<T>,
    charge: Charge,
}
impl<T> MemoryVec<T> {
    pub fn new(budget: &Arc<Budget>, capacity: usize) -> Result<Self, Error> {
        let requested = capacity
            .checked_mul(std::mem::size_of::<T>())
            .ok_or(Error::Limit)?;
        let bytes = requested
            .checked_add(std::mem::size_of::<Self>())
            .ok_or(Error::Limit)?;
        let mut charge = budget.charge(bytes)?;
        let mut data = Vec::new();
        data.try_reserve_exact(capacity)
            .map_err(|_| Error::Allocation)?;
        let actual = data
            .capacity()
            .checked_mul(std::mem::size_of::<T>())
            .ok_or(Error::Limit)?;
        // A refused allocator over-allocation is dropped before becoming retained data.
        charge.additional(actual.saturating_sub(requested))?;
        Ok(Self { data, charge })
    }
    pub fn capacity(&self) -> usize {
        self.data.capacity()
    }
    pub fn push(&mut self, value: T) -> Result<(), T> {
        if self.data.len() == self.data.capacity() {
            return Err(value);
        }
        self.data.push(value);
        Ok(())
    }
    pub fn retain(&mut self, keep: impl FnMut(&T) -> bool) {
        self.data.retain(keep);
    }
    pub fn remove(&mut self, index: usize) -> Option<T> {
        if index >= self.data.len() {
            return None;
        }
        Some(self.data.remove(index))
    }
    pub fn pop(&mut self) -> Option<T> {
        self.data.pop()
    }
    pub fn truncate(&mut self, len: usize) {
        self.data.truncate(len);
    }
    pub fn clear(&mut self) {
        self.data.clear();
    }
    /// Retain only used elements, charging both buffers during the move.
    pub fn compact(&mut self) -> Result<(), Error> {
        if self.data.len() == self.data.capacity() {
            return Ok(());
        }
        let mut next = Self::new(&self.charge.budget, self.data.len())?;
        next.data.append(&mut self.data);
        std::mem::swap(self, &mut next);
        Ok(())
    }
    /// Charge both allocations during growth; a refused reserve leaves data intact.
    pub fn reserve(&mut self, capacity: usize) -> Result<(), Error> {
        if capacity <= self.data.capacity() {
            return Ok(());
        }
        let mut next = Self::new(&self.charge.budget, capacity)?;
        // New capacity exceeds the old capacity, so these pushes cannot allocate.
        next.data.append(&mut self.data);
        std::mem::swap(self, &mut next);
        Ok(())
    }
}
impl<T> std::ops::Deref for MemoryVec<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.data
    }
}
impl<T> std::ops::DerefMut for MemoryVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.data
    }
}
#[derive(Debug)]
pub struct MemoryString {
    text: String,
    charge: Charge,
}
impl MemoryString {
    pub fn new(budget: &Arc<Budget>, text: &str) -> Result<Self, Error> {
        let bytes = text
            .len()
            .checked_add(std::mem::size_of::<Self>())
            .ok_or(Error::Limit)?;
        let mut charge = budget.charge(bytes)?;
        let mut owned = String::new();
        owned
            .try_reserve_exact(text.len())
            .map_err(|_| Error::Allocation)?;
        charge.additional(owned.capacity().saturating_sub(text.len()))?;
        owned.push_str(text);
        Ok(Self {
            text: owned,
            charge,
        })
    }
    pub fn as_str(&self) -> &str {
        &self.text
    }
    pub fn storage_bytes(&self) -> usize {
        self.charge.bytes
    }
}
