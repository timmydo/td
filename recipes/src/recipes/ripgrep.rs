use crate::types::Recipe;

// Built on demand by the real `td shell` product path. The package source and
// Cargo.lock-selected registry closure are fixed, checksum-verified inputs
// provisioned by td-feed; `td shell` supplies the source-built stage2 Rust and
// native GCC/binutils/glibc recipe outputs as the build platform.
pub fn recipe() -> Recipe {
    Recipe::rust("ripgrep", "14.1.1").bins(&["rg"])
}
