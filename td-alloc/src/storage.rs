use crate::{raw, Budget, BudgetError, Charge};
use std::fmt;
use std::ops::{Deref, DerefMut};

fn growth_capacity<T>(budget: &Budget, current: usize, needed: usize) -> usize {
    let minimum = match std::mem::size_of::<T>() {
        1 => 8,
        2..=1024 => 4,
        _ => 1,
    };
    let preferred = current.saturating_mul(2).max(minimum).max(needed);
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
        // Conservatively admit the full replacement even when realloc can
        // extend in place. On refusal the old allocation and charge survive.
        let layout = std::alloc::Layout::array::<T>(capacity).map_err(|_| BudgetError::Overflow)?;
        let charge = self.budget().charge(layout.size())?;
        raw::grow_vector(&mut self.values, capacity)?;
        self.charge = charge;
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
    #[inline]
    pub fn try_push(&mut self, value: T) -> Result<(), BudgetError> {
        self.try_reserve(1)?;
        self.values.push(value);
        Ok(())
    }
    /// Remove an element without allocating; an invalid index changes nothing.
    pub fn remove(&mut self, index: usize) -> Option<T> {
        if index < self.len() {
            Some(self.values.remove(index))
        } else {
            None
        }
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
    #[inline]
    fn deref(&self) -> &[T] {
        &self.values
    }
}
impl<T> DerefMut for BudgetVec<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.values
    }
}

impl<T: fmt::Debug> fmt::Debug for BudgetVec<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.values.fmt(f)
    }
}
impl<T: PartialEq> PartialEq for BudgetVec<T> {
    fn eq(&self, other: &Self) -> bool {
        self.values == other.values
    }
}
impl<T: Eq> Eq for BudgetVec<T> {}
impl<'a, T> IntoIterator for &'a BudgetVec<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
impl<'a, T> IntoIterator for &'a mut BudgetVec<T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

/// Fixed-length, movable backing for parsers. Neither growth nor uncharged
/// ownership extraction is exposed. Views remain borrowed from this owner.
pub struct Buffer<T>(BudgetVec<T>);
impl<T: Copy> Buffer<T> {
    pub fn copy(budget: &Budget, values: &[T]) -> Result<Self, BudgetError> {
        let mut out = BudgetVec::with_capacity(budget, values.len())?;
        out.try_extend_from_slice(values)?;
        Ok(out.freeze())
    }
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
    #[inline]
    fn deref(&self) -> &[T] {
        &self.0
    }
}
impl<T> DerefMut for Buffer<T> {
    #[inline]
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
    pub fn copy(budget: &Budget, text: &str) -> Result<Self, BudgetError> {
        let mut out = Self::with_capacity(budget, text.len())?;
        out.try_push_str(text)?;
        Ok(out)
    }
    pub fn try_clone(&self) -> Result<Self, BudgetError> {
        Self::copy(self.budget(), self.as_str())
    }
    #[inline]
    pub fn try_push(&mut self, value: char) -> Result<(), BudgetError> {
        if self.values.capacity() - self.values.len() >= value.len_utf8() {
            self.values.push(value);
            Ok(())
        } else {
            self.try_push_str(value.encode_utf8(&mut [0; 4]))
        }
    }
    pub fn capacity(&self) -> usize {
        self.values.capacity()
    }

    pub fn new(budget: &Budget) -> Result<Self, BudgetError> {
        Self::with_capacity(budget, 0)
    }
    pub fn with_capacity(budget: &Budget, capacity: usize) -> Result<Self, BudgetError> {
        let BudgetVec { values, charge } = BudgetVec::<u8>::with_capacity(budget, capacity)?;
        // Empty, valid UTF-8. String adopts the exact capacity without copying.
        let values = String::from_utf8(values).map_err(|_| BudgetError::Overflow)?;
        Ok(Self { values, charge })
    }
    #[inline]
    pub fn try_push_str(&mut self, value: &str) -> Result<(), BudgetError> {
        let needed = self
            .values
            .len()
            .checked_add(value.len())
            .ok_or(BudgetError::Overflow)?;
        if needed > self.values.capacity() {
            let preferred = growth_capacity::<u8>(self.budget(), self.values.capacity(), needed);
            let capacity = match self.budget().charge(preferred) {
                Err(BudgetError::Limit | BudgetError::Overflow) if preferred != needed => {
                    self.budget().charge(needed)?
                }
                result => result?,
            };
            raw::grow_string(&mut self.values, capacity.bytes())?;
            self.charge = capacity;
        }
        self.values.push_str(value);
        Ok(())
    }
    #[inline]
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

impl Deref for BudgetString {
    type Target = str;
    #[inline]
    fn deref(&self) -> &str {
        self.as_str()
    }
}
impl fmt::Debug for BudgetString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.values.fmt(f)
    }
}
impl fmt::Display for BudgetString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.values.fmt(f)
    }
}
impl PartialEq for BudgetString {
    fn eq(&self, other: &Self) -> bool {
        self.values == other.values
    }
}
impl PartialEq<str> for BudgetString {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}
impl PartialEq<&str> for BudgetString {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}
impl Eq for BudgetString {}

/// Fallible immutable shared ownership. The control block and value stay
/// charged until the final owner drops; nested allocations need owned storage.
pub struct BudgetShared<T>(raw::Shared<SharedValue<T>>);
struct SharedValue<T> {
    value: T,
    charge: Charge,
}
impl<T> BudgetShared<T> {
    pub fn new(budget: &Budget, value: T) -> Result<Self, BudgetError> {
        let charge = budget.charge(raw::Shared::<SharedValue<T>>::bytes())?;
        Ok(Self(raw::Shared::new(SharedValue { value, charge })?))
    }
    pub fn try_clone(&self) -> Result<Self, BudgetError> {
        Ok(Self(self.0.try_clone()?))
    }
    pub fn budget(&self) -> &Budget {
        self.0.get().charge.budget()
    }
}
impl<T> Deref for BudgetShared<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0.get().value
    }
}
impl<T: fmt::Debug> fmt::Debug for BudgetShared<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.deref().fmt(f)
    }
}
impl<T: PartialEq> PartialEq for BudgetShared<T> {
    fn eq(&self, other: &Self) -> bool {
        self.deref() == other.deref()
    }
}
impl<T: Eq> Eq for BudgetShared<T> {}
impl PartialEq<&str> for BudgetShared<BudgetString> {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
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
