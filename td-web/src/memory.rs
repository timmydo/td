//! Page admission follows owners, including workers and replaced outputs.
use td_alloc::{Budget, BudgetError, Charge};

pub(crate) const PAGES: usize = 320 << 20;
/// A page's terms (DESIGN.md §10, "Memory"): its tree, styled document
/// and layouts; its sheets; its inline pictures. They sum to `PAGES`.
pub(crate) const PAGE: usize = td_layout::MAX_PAGE_BYTES;
pub(crate) const SHEETS: usize = 48 << 20;
pub(crate) const PICTURES: usize = 48 << 20;
const _: () = assert!(PAGE + SHEETS + PICTURES == PAGES);
pub(crate) const PICTURE_WORK: usize = 200 << 20;

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

    /// DESIGN.md §10, "The jail": what td-alloc holds (the root, `PAGES`
    /// and its control blocks) and the fixed terms beside it stay at least
    /// 20 MB under the jail's `memory-high`.
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn the_page_budgets_fit_the_jail() {
        // The window at 3840x2160, §3's measured 144 MB rounded up, and the
        // jail's memory-high: both copied by hand and reviewed.
        const WINDOW: usize = 145_000_000;
        const MEMORY_HIGH: usize = 640 << 20;
        // The subresource bodies in flight, before the render thread
        // decodes them: a sheet's at most 1 MiB, a picture's (increment
        // 9) at most MAX_SUBRESOURCE, held in reserve before it exists.
        const MAX_SUBRESOURCE: usize = 4 << 20;
        assert!(crate::gather::MAX_SHEET <= MAX_SUBRESOURCE);
        let subresources = crate::subresource::MAX_SUBRESOURCES * MAX_SUBRESOURCE;
        assert_eq!(PAGE + SHEETS + PICTURES, PAGES);
        // A page's gathered sheets and the set compiled from them share
        // the sheets term.
        assert!(crate::gather::MAX_HELD < SHEETS);
        assert_eq!(PAGE, td_layout::MAX_PAGE_BYTES);
        assert!(td_html::html5::MAX_TREE_BYTES < PAGE);
        // A navigated picture's peak, its decoders' bulk and the table
        // overhead, fits the root.
        assert!(PICTURE_WORK >= td_ui::picture::MAX_PICTURE_PIXELS * 12);
        assert!(PICTURE_WORK <= PAGES);
        let root = root().unwrap();
        assert_eq!(root.limit(), PAGES + 16 * Budget::metadata_bytes());
        let bodies = crate::app::MAX_WORKERS * crate::fetch::MAX_DOCUMENT as usize;
        let total = WINDOW
            + td_ui::prose::ATLAS
            + bodies
            + subresources
            + td_layout::WORKING_BYTES
            + root.limit();
        assert!(total + 20_000_000 <= MEMORY_HIGH, "{total}");
    }
}
