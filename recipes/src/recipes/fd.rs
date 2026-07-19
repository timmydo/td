use crate::types::{Recipe, SourcePin};

// Keep fd's C-building jemalloc default disabled. The completions feature is
// pure Rust and preserves the prior shipped package configuration while the
// build remains wholly inside the declared td toolchain and offline crate set.
pub fn recipe() -> Recipe {
    Recipe::rust("fd", "10.2.0")
        .source_pin(SourcePin::new(
            "fd-source",
            "https://static.crates.io/crates/fd-find/fd-find-10.2.0.crate",
            "de08defa195af894cc295a43bfc65ba28903e492fd5f32f7a24bf75eafd9bf34",
            "fd-find-10.2.0.crate",
        ))
        .bins(&["fd"])
        .no_default_features()
        .features(&["completions"])
}
