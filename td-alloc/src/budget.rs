use crate::raw::Shared;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetError {
    Limit,
    Overflow,
    Allocation,
}
impl From<crate::Error> for BudgetError {
    fn from(_: crate::Error) -> Self {
        Self::Allocation
    }
}
impl fmt::Display for BudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Limit => "memory budget exhausted",
            Self::Overflow => "allocation size or ownership count overflow",
            Self::Allocation => "allocator refused storage",
        })
    }
}
impl std::error::Error for BudgetError {}

/// Shared budget identity. Every child and allocation keeps its ancestors alive.
/// Counters charge requested layouts, including budget metadata, not RSS.
pub struct Budget(Shared<State>);
struct State {
    parent: Option<Budget>,
    limit: usize,
    used: AtomicUsize,
    depth: usize,
}
impl Drop for State {
    fn drop(&mut self) {
        if let Some(parent) = &self.parent {
            parent.release(Budget::metadata_bytes());
        }
    }
}
impl Budget {
    pub const MAX_DEPTH: usize = 16;
    pub const fn metadata_bytes() -> usize {
        Shared::<State>::bytes()
    }

    pub fn new(limit: usize) -> Result<Self, BudgetError> {
        Self::create(None, limit, 1)
    }
    pub fn child(&self, limit: usize) -> Result<Self, BudgetError> {
        let depth = self
            .0
            .get()
            .depth
            .checked_add(1)
            .ok_or(BudgetError::Overflow)?;
        if depth > Self::MAX_DEPTH {
            return Err(BudgetError::Limit);
        }
        Self::create(Some(self.try_clone()?), limit, depth)
    }
    fn create(parent: Option<Budget>, limit: usize, depth: usize) -> Result<Self, BudgetError> {
        let bytes = Self::metadata_bytes();
        if limit < bytes {
            return Err(BudgetError::Limit);
        }
        if let Some(parent) = &parent {
            parent.acquire(bytes)?;
        }
        // State's destructor returns the parent's metadata charge on allocation
        // refusal too. No counter is replenished while an owner can still use it.
        Ok(Self(Shared::new(State {
            parent,
            limit,
            used: AtomicUsize::new(bytes),
            depth,
        })?))
    }
    pub fn try_clone(&self) -> Result<Self, BudgetError> {
        Ok(Self(self.0.try_clone()?))
    }
    pub fn limit(&self) -> usize {
        self.0.get().limit
    }
    /// Includes conservative admissions in progress on other workers.
    pub fn used(&self) -> usize {
        self.0.get().used.load(Ordering::Relaxed)
    }
    // A sizing hint only: concurrent admission can change any node after its
    // load. Storage must still acquire the complete replacement charge.
    pub(crate) fn remaining(&self) -> usize {
        let state = self.0.get();
        let remaining = state.limit.saturating_sub(self.used());
        match &state.parent {
            Some(parent) => remaining.min(parent.remaining()),
            None => remaining,
        }
    }
    pub fn charge(&self, bytes: usize) -> Result<Charge, BudgetError> {
        let budget = self.try_clone()?;
        budget.acquire(bytes)?;
        Ok(Charge { budget, bytes })
    }
    fn acquire(&self, bytes: usize) -> Result<(), BudgetError> {
        let state = self.0.get();
        state
            .used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= state.limit)
            })
            .map_err(|_| BudgetError::Limit)?;
        if let Some(parent) = &state.parent {
            if let Err(error) = parent.acquire(bytes) {
                state.used.fetch_sub(bytes, Ordering::Relaxed);
                return Err(error);
            }
        }
        Ok(())
    }
    fn release(&self, bytes: usize) {
        self.0.get().used.fetch_sub(bytes, Ordering::Relaxed);
        if let Some(parent) = &self.0.get().parent {
            parent.release(bytes);
        }
    }
}

/// A non-cloneable reservation. Dropping it returns bytes to the entire chain.
/// Reserve before acquiring an external resource and retain until after release.
pub struct Charge {
    budget: Budget,
    bytes: usize,
}
impl Charge {
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn budget(&self) -> &Budget {
        &self.budget
    }
}
impl Drop for Charge {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}
