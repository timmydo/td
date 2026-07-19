use crate::types::Recipe;

// Keep fd's C-building jemalloc default disabled. The completions feature is
// pure Rust and preserves the prior shipped package configuration while the
// build remains wholly inside the declared td toolchain and offline crate set.
pub fn recipe() -> Recipe {
    Recipe::rust("fd", "10.2.0")
        .bins(&["fd"])
        .no_default_features()
        .features(&["completions"])
}
