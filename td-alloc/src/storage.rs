use crate::{raw, Budget, BudgetError, Charge};
use std::ops::{Deref, DerefMut};

fn growth_capacity<T>(budget: &Budget, current: usize, needed: usize) -> usize {
    let preferred = current.saturating_mul(2).max(8).max(needed);
    let available = budget
        .remaining()
        .min(isize::MAX as usize)
        .checked_div(std::mem::size_of::<T>())
        .unwrap_or(usize::MAX);
    // Take usable spare capacity together instead of repeatedly reallocating
    // one element near a stable cap. This hint never replaces admission, and
    // needed still gets an attempt if a concurrent owner returns its storage.
    preferred.min(available).max(needed)
}

/// Owned storage whose capacity is admitted before allocating. Only slice
/// access escapes; growth and ownership retain the original budget.
pub struct BudgetVec<T> {
    // Field order is part of the contract: free storage before releasing charge.
    values: Vec<T>,
    charge: Charge,
}
impl<T> BudgetVec<T> {
    pub fn new(budget: &Budget) -> Result<Self, BudgetError> {
        Self::with_capacity(budget, 0)
    }
    pub fn with_capacity(budget: &Budget, capacity: usize) -> Result<Self, BudgetError> {
        let layout = std::alloc::Layout::array::<T>(capacity).map_err(|_| BudgetError::Overflow)?;
        let charge = budget.charge(layout.size())?;
        let values = raw::vector(capacity)?;
        Ok(Self { values, charge })
    }
    pub fn capacity(&self) -> usize {
        self.values.capacity()
    }
    pub fn budget(&self) -> &Budget {
        self.charge.budget()
    }
    pub fn try_reserve_exact(&mut self, additional: usize) -> Result<(), BudgetError> {
        let capacity = self
            .len()
            .checked_add(additional)
            .ok_or(BudgetError::Overflow)?;
        if capacity <= self.capacity() {
            return Ok(());
        }
        // Both buffers are charged during the copy, even if the allocator could
        // have extended in place. Refusal leaves contents and capacity unchanged.
        let mut next = Self::with_capacity(self.budget(), capacity)?;
        next.values.append(&mut self.values);
        *self = next;
        Ok(())
    }
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), BudgetError> {
        let needed = self
            .len()
            .checked_add(additional)
            .ok_or(BudgetError::Overflow)?;
        if needed <= self.capacity() {
            return Ok(());
        }
        let preferred = growth_capacity::<T>(self.budget(), self.capacity(), needed);
        match self.try_reserve_exact(preferred - self.len()) {
            Err(BudgetError::Limit | BudgetError::Overflow) if preferred != needed => {
                self.try_reserve_exact(additional)
            }
            result => result,
        }
    }
    pub fn try_push(&mut self, value: T) -> Result<(), BudgetError> {
        self.try_reserve(1)?;
        self.values.push(value);
        Ok(())
    }
    pub fn pop(&mut self) -> Option<T> {
        self.values.pop()
    }
    pub fn clear(&mut self) {
        self.values.clear();
    }
    pub fn truncate(&mut self, len: usize) {
        self.values.truncate(len);
    }
    pub fn freeze(self) -> Buffer<T> {
        Buffer(self)
    }
}
impl<T: Copy> BudgetVec<T> {
    pub fn filled(budget: &Budget, len: usize, value: T) -> Result<Self, BudgetError> {
        let mut out = Self::with_capacity(budget, len)?;
        out.values.resize_with(len, || value);
        Ok(out)
    }
    pub fn try_extend_from_slice(&mut self, values: &[T]) -> Result<(), BudgetError> {
        self.try_reserve(values.len())?;
        // Deliberately do not invoke a user-defined Clone implementation.
        for &value in values {
            self.values.push(value);
        }
        Ok(())
    }
}
impl<T> Deref for BudgetVec<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.values
    }
}
impl<T> DerefMut for BudgetVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.values
    }
}

/// Fixed-length, movable backing for parsers. Neither growth nor uncharged
/// ownership extraction is exposed. Views remain borrowed from this owner.
pub struct Buffer<T>(BudgetVec<T>);
impl<T: Copy> Buffer<T> {
    pub fn filled(budget: &Budget, len: usize, value: T) -> Result<Self, BudgetError> {
        Ok(BudgetVec::filled(budget, len, value)?.freeze())
    }
}
impl<T> Buffer<T> {
    pub fn budget(&self) -> &Budget {
        self.0.budget()
    }
    pub fn into_vec(self) -> BudgetVec<T> {
        self.0
    }
}
impl<T> Deref for Buffer<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.0
    }
}
impl<T> DerefMut for Buffer<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.0
    }
}

/// UTF-8 storage with fallible growth and no mutable byte/Vec escape hatch.
pub struct BudgetString {
    values: String,
    charge: Charge,
}
impl BudgetString {
    pub fn new(budget: &Budget) -> Result<Self, BudgetError> {
        Self::with_capacity(budget, 0)
    }
    pub fn with_capacity(budget: &Budget, capacity: usize) -> Result<Self, BudgetError> {
        let BudgetVec { values, charge } = BudgetVec::<u8>::with_capacity(budget, capacity)?;
        // Empty, valid UTF-8. String adopts the exact capacity without copying.
        let values = String::from_utf8(values).map_err(|_| BudgetError::Overflow)?;
        Ok(Self { values, charge })
    }
    pub fn try_push_str(&mut self, value: &str) -> Result<(), BudgetError> {
        let needed = self
            .values
            .len()
            .checked_add(value.len())
            .ok_or(BudgetError::Overflow)?;
        if needed > self.values.capacity() {
            let preferred = growth_capacity::<u8>(self.budget(), self.values.capacity(), needed);
            let mut next = match Self::with_capacity(self.budget(), preferred) {
                Err(BudgetError::Limit | BudgetError::Overflow) if preferred != needed => {
                    Self::with_capacity(self.budget(), needed)?
                }
                result => result?,
            };
            next.values.push_str(&self.values);
            *self = next;
        }
        self.values.push_str(value);
        Ok(())
    }
    pub fn as_str(&self) -> &str {
        &self.values
    }
    pub fn clear(&mut self) {
        self.values.clear();
    }
    pub fn budget(&self) -> &Budget {
        self.charge.budget()
    }
}

/// Typed arena with stable indices, not stable addresses. Values are destroyed
/// together on drop; references borrow the arena and prevent simultaneous growth.
/// Indices belong to this arena and are ordinary bounds-checked offsets.
pub struct Arena<T>(BudgetVec<T>);
impl<T> Arena<T> {
    pub fn new(budget: &Budget) -> Result<Self, BudgetError> {
        Ok(Self(BudgetVec::new(budget)?))
    }
    pub fn with_capacity(budget: &Budget, capacity: usize) -> Result<Self, BudgetError> {
        Ok(Self(BudgetVec::with_capacity(budget, capacity)?))
    }
    pub fn try_insert(&mut self, value: T) -> Result<usize, BudgetError> {
        let index = self.0.len();
        self.0.try_push(value)?;
        Ok(index)
    }
    pub fn get(&self, index: usize) -> Option<&T> {
        self.0.get(index)
    }
    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.0.get_mut(index)
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
