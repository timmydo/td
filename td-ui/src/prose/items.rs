//! Optional owned storage makes an empty line allocation-free. A default
//! value cannot grow; a funded value retains its original budget on every move.
use std::ops::{Deref, DerefMut};
use td_alloc::{Budget, BudgetError, BudgetVec};

#[derive(Debug)]
pub struct Items<T>(Option<BudgetVec<T>>);
impl<T> Default for Items<T> {
    fn default() -> Self {
        Self(None)
    }
}
impl<T> Items<T> {
    pub fn new(budget: &Budget) -> Result<Self, BudgetError> {
        Ok(Self(Some(BudgetVec::new(budget)?)))
    }
    pub fn try_with_capacity(budget: &Budget, capacity: usize) -> Result<Self, BudgetError> {
        Ok(Self(Some(td_alloc::BudgetVec::with_capacity(
            budget, capacity,
        )?)))
    }
    pub fn capacity(&self) -> usize {
        self.0.as_ref().map_or(0, BudgetVec::capacity)
    }
    pub fn budget(&self) -> Option<&Budget> {
        self.0.as_ref().map(BudgetVec::budget)
    }
    pub fn try_reserve(&mut self, n: usize) -> Result<(), BudgetError> {
        td_alloc::BudgetVec::try_reserve(self.0.as_mut().ok_or(BudgetError::Limit)?, n)
    }
    #[inline]
    pub fn try_push(&mut self, value: T) -> Result<(), BudgetError> {
        self.0.as_mut().ok_or(BudgetError::Limit)?.try_push(value)
    }
    pub fn pop(&mut self) -> Option<T> {
        self.0.as_mut()?.pop()
    }
    pub fn remove(&mut self, at: usize) -> Option<T> {
        self.0.as_mut()?.remove(at)
    }
    pub fn clear(&mut self) {
        if let Some(v) = &mut self.0 {
            v.clear();
        }
    }
    pub fn truncate(&mut self, len: usize) {
        if let Some(v) = &mut self.0 {
            v.truncate(len);
        }
    }
    pub fn try_clone_with(
        &self,
        mut copy: impl FnMut(&T) -> Result<T, BudgetError>,
    ) -> Result<Self, BudgetError> {
        let Some(budget) = self.budget() else {
            return Ok(Self::default());
        };
        let mut out = Self::try_with_capacity(budget, self.len())?;
        for value in self.iter() {
            out.try_push(copy(value)?)?;
        }
        Ok(out)
    }
}
impl<T> Deref for Items<T> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] {
        self.0.as_deref().unwrap_or(&[])
    }
}
impl<T> DerefMut for Items<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        self.0.as_deref_mut().unwrap_or(&mut [])
    }
}
impl<'a, T> IntoIterator for &'a Items<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
impl<'a, T> IntoIterator for &'a mut Items<T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}
impl<T> IntoIterator for Items<T> {
    type Item = T;
    type IntoIter = std::iter::Flatten<std::option::IntoIter<BudgetVec<T>>>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter().flatten()
    }
}

impl<T: PartialEq> PartialEq for Items<T> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}
impl<T: Eq> Eq for Items<T> {}
