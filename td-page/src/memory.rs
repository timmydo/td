//! A page's memory (td-web/DESIGN.md §10, "Memory"): the terms a page is
//! made in, and storage admitted to a budget before the work that fills
//! it.
use td_alloc::{Budget, BudgetError, Charge};

/// A page's terms: its tree, styled document and layouts; its sheets; its
/// inline pictures. They sum to `PAGES`.
pub const PAGE: usize = td_layout::MAX_PAGE_BYTES;
pub const SHEETS: usize = 48 << 20;
pub const PICTURES: usize = 48 << 20;
pub const PAGES: usize = 320 << 20;
const _: () = assert!(PAGE + SHEETS + PICTURES == PAGES);
// A page's gathered sheets and the set compiled from them share the
// sheets term; its parsed tree fits the page's.
const _: () = assert!(crate::gather::MAX_HELD < SHEETS);
const _: () = assert!(td_html::html5::MAX_TREE_BYTES < PAGE);

/// Storage admitted before external work. Field order frees the value first.
pub struct Resident<T> {
    value: T,
    charge: Charge,
}

impl<T> Resident<T> {
    /// `value` holding `charge`, shrunk to the `retained` bytes it keeps.
    pub fn new(value: T, charge: Charge, retained: usize) -> Result<Self, BudgetError> {
        let mut out = Self { value, charge };
        Charge::shrink_to(&mut out.charge, retained)?;
        Ok(out)
    }

    pub fn budget(&self) -> &Budget {
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
