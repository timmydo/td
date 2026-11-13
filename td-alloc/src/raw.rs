//! The allocation boundary recorded in UNSAFE.md. No public raw pointers.
use crate::BudgetError;
use std::alloc::{alloc, dealloc, realloc, Layout};
use std::ptr::NonNull;
use std::sync::atomic::{fence, AtomicUsize, Ordering};

/// Exact requested layout, with zero initialized elements. Vec owns destruction.
#[allow(unsafe_code)]
pub(super) fn vector<T>(capacity: usize) -> Result<Vec<T>, BudgetError> {
    let layout = Layout::array::<T>(capacity).map_err(|_| BudgetError::Overflow)?;
    if layout.size() == 0 {
        return Ok(Vec::new());
    }
    crate::helpers::admit()?;
    // SAFETY: nonzero validated layout; null is returned as an error. The
    // allocation has exactly Vec's global allocator layout and no initialized
    // elements. Ownership transfers once, including the exact element capacity.
    unsafe {
        let ptr = NonNull::new(alloc(layout).cast::<T>()).ok_or(BudgetError::Allocation)?;
        Ok(Vec::from_raw_parts(ptr.as_ptr(), 0, capacity))
    }
}

/// Grow an exclusively owned Vec without moving its initialized values twice.
#[allow(unsafe_code)]
pub(super) fn grow_vector<T>(values: &mut Vec<T>, capacity: usize) -> Result<(), BudgetError> {
    if capacity <= values.capacity() {
        return Ok(());
    }
    let next = Layout::array::<T>(capacity).map_err(|_| BudgetError::Overflow)?;
    if values.capacity() == 0 {
        *values = vector(capacity)?;
        return Ok(());
    }
    let old = Layout::array::<T>(values.capacity()).map_err(|_| BudgetError::Overflow)?;
    let len = values.len();
    crate::helpers::admit()?;
    // SAFETY: ZST Vec capacity is usize::MAX and returned above. Both layouts
    // are nonzero, validated arrays with identical alignment. The exclusive
    // Vec owns exactly the old global allocation. On null it remains valid.
    // On success realloc preserves all initialized bytes and consumes the old
    // allocation. Replace only its stack descriptor, without dropping the old
    // descriptor or invoking any user code, then resume normal Vec ownership.
    unsafe {
        let pointer =
            NonNull::new(realloc(values.as_mut_ptr().cast(), old, next.size()).cast::<T>())
                .ok_or(BudgetError::Allocation)?;
        std::ptr::write(values, Vec::from_raw_parts(pointer.as_ptr(), len, capacity));
    }
    Ok(())
}

#[allow(unsafe_code)]
pub(super) fn grow_string(values: &mut String, capacity: usize) -> Result<(), BudgetError> {
    // SAFETY: grow_vector preserves the length and all initialized bytes on
    // success and failure. It cannot invalidate the String's UTF-8 invariant.
    unsafe { grow_vector(values.as_mut_vec(), capacity) }
}

struct Inner<T> {
    references: AtomicUsize,
    value: T,
}

/// Strong ownership only. Cloning is checked and never allocates; no weak refs,
/// mutation, raw conversions or infallible Clone implementation are exposed.
pub(super) struct Shared<T>(NonNull<Inner<T>>);

// SAFETY: only shared references to T escape. Atomic reference counting keeps
// the allocation alive; the final owner alone destroys T and the allocation.
#[allow(unsafe_code)]
unsafe impl<T: Send + Sync> Send for Shared<T> {}
#[allow(unsafe_code)]
unsafe impl<T: Send + Sync> Sync for Shared<T> {}

impl<T> Shared<T> {
    pub(super) const fn bytes() -> usize {
        std::mem::size_of::<Inner<T>>()
    }

    #[allow(unsafe_code)]
    pub(super) fn new(value: T) -> Result<Self, BudgetError> {
        crate::helpers::admit()?;
        // SAFETY: Inner includes an AtomicUsize and is nonzero sized. Initialize
        // exactly once before publishing; on null, value is dropped normally.
        unsafe {
            let ptr = NonNull::new(alloc(Layout::new::<Inner<T>>()).cast::<Inner<T>>())
                .ok_or(BudgetError::Allocation)?;
            ptr.as_ptr().write(Inner {
                references: AtomicUsize::new(1),
                value,
            });
            Ok(Self(ptr))
        }
    }

    #[allow(unsafe_code)]
    fn inner(&self) -> &Inner<T> {
        // SAFETY: this handle holds a strong reference throughout the borrow.
        unsafe { self.0.as_ref() }
    }

    pub(super) fn get(&self) -> &T {
        &self.inner().value
    }

    pub(super) fn try_clone(&self) -> Result<Self, BudgetError> {
        self.inner()
            .references
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                n.checked_add(1).filter(|n| *n <= isize::MAX as usize)
            })
            .map_err(|_| BudgetError::Overflow)?;
        Ok(Self(self.0))
    }
}

impl<T> Drop for Shared<T> {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        if self.inner().references.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        fence(Ordering::Acquire);
        // SAFETY: we removed the last reference and synchronized with all prior
        // owners. No Pin or address guarantee escapes this private owner. Move
        // the value out, free its exact layout, THEN drop it: State's destructor
        // returns the parent's metadata charge, which must remain live until
        // this allocation is freed. A panicking destructor cannot leak the block.
        unsafe {
            let inner = self.0.as_ptr().read();
            dealloc(self.0.as_ptr().cast(), Layout::new::<Inner<T>>());
            #[cfg(test)]
            tests::DEALLOCATIONS.with(|count| count.set(count.get() + 1));
            drop(inner);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    thread_local! {
        pub(super) static DEALLOCATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    #[test]
    fn control_block_is_freed_before_value_returns_its_charge() {
        struct Check(usize);
        impl Drop for Check {
            fn drop(&mut self) {
                DEALLOCATIONS.with(|count| assert_eq!(count.get(), self.0 + 1));
            }
        }
        let before = DEALLOCATIONS.with(|count| count.get());
        drop(Shared::new(Check(before)).unwrap());
    }

    #[test]
    fn reference_overflow_refuses_without_wrapping() {
        let value = Shared::new(42).unwrap();
        value
            .inner()
            .references
            .store(isize::MAX as usize, Ordering::Relaxed);
        assert!(matches!(value.try_clone(), Err(BudgetError::Overflow)));
        assert_eq!(
            value.inner().references.load(Ordering::Relaxed),
            isize::MAX as usize
        );
        // Restore this fixture's actual single owner before dropping it.
        value.inner().references.store(1, Ordering::Relaxed);
    }
}
