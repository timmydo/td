//! Page admission follows owners, including workers and replaced outputs.
//! A page's terms are td-page's (`td_page::memory`); the root holds them
//! and their control blocks.
use td_alloc::{Budget, BudgetError};

pub(crate) use td_page::memory::{PAGES, PICTURES};
pub(crate) const PICTURE_WORK: usize = 200 << 20;

pub(crate) fn root() -> Result<Budget, BudgetError> {
    Budget::new(PAGES + 16 * Budget::metadata_bytes())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use td_page::memory::{PAGE, SHEETS};
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
        assert!(td_page::gather::MAX_SHEET <= MAX_SUBRESOURCE);
        let subresources = crate::subresource::MAX_SUBRESOURCES * MAX_SUBRESOURCE;
        // td-page holds a page's terms to their sum and the tree and
        // sheets to their terms.
        assert_eq!(PAGE + SHEETS + PICTURES, PAGES);
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
