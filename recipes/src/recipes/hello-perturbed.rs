use crate::types::Recipe;

// The self-discrimination twin for the hello recipe check: a LOAD-BEARING recipe
// field (configureFlags) differs from base `hello`, so it assembles a DISTINCT .drv
// even though the source is resolved from the lock (a source-hash perturbation would
// be vacuous in the build-recipe path.
pub fn recipe() -> Recipe {
    let mut r = super::hello::recipe();
    r.checks = None;
    r.configure_flags(&["--disable-nls"])
}
