//! Page admission follows owners, including workers and replaced outputs.
use td_alloc::{Budget, BudgetError, Charge};

pub(crate) const PAGES: usize = 320 << 20;
// These are the legacy renderer's separate conservative working allowances,
// not exact allocator/RSS measurements. See DESIGN.md §3.
pub(crate) const HTML_WORK: usize = 256 << 20;
pub(crate) const TEXT_WORK: usize = 64 << 20;
pub(crate) const WRAP_WORK: usize = 24 << 20;
pub(crate) const PICTURE_WORK: usize = 200 << 20;
pub(crate) const SMALL_WORK: usize = 1 << 20;

pub(crate) fn root() -> Result<Budget, BudgetError> {
    Budget::new(PAGES + 16 * Budget::metadata_bytes())
}

/// Storage admitted before external work. Field order frees the value first.
pub(crate) struct Resident<T> {
    value: T,
    charge: Charge,
}
impl<T> Resident<T> {
    pub(crate) fn new(value: T, charge: Charge, retained: usize) -> Result<Self, BudgetError> {
        let mut out = Self { value, charge };
        Charge::shrink_to(&mut out.charge, retained)?;
        Ok(out)
    }
    pub(crate) fn budget(&self) -> &Budget {
        self.charge.budget()
    }
}
impl<T> std::ops::Deref for Resident<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}
impl<T: std::fmt::Debug> std::fmt::Debug for Resident<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.fmt(f)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn failed_shrink_and_final_drop_free_values_before_credit() {
        struct Value<'a>(&'a Budget, usize);
        impl Drop for Value<'_> {
            fn drop(&mut self) {
                assert!(self.0.used() >= self.1);
            }
        }
        let root = Budget::new(1024).unwrap();
        let base = root.used();
        assert!(Resident::new(Value(&root, base + 128), root.charge(128).unwrap(), 129).is_err());
        assert_eq!(root.used(), base);
        let resident =
            Resident::new(Value(&root, base + 64), root.charge(128).unwrap(), 64).unwrap();
        assert_eq!(root.used(), base + 64);
        drop(resident);
        assert_eq!(root.used(), base);
    }
}
