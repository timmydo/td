use crate::types::Recipe;

// The self-discrimination twin for the corpus-no-guix gate: a LOAD-BEARING recipe
// field (configureFlags) differs from base `hello`, so it assembles a DISTINCT .drv
// even though the source is resolved from the lock (a source-hash perturbation would
// be vacuous in the build-recipe path — see mk/gates/220-corpus-no-guix.mk).
pub fn recipe() -> Recipe {
    super::hello::recipe().configure_flags(&["--disable-nls"])
}
