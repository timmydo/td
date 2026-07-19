use crate::types::{Recipe, SourcePin};

// Built on demand by the real `td shell` product path. The package source and
// Cargo.lock-selected registry closure are fixed, checksum-verified inputs
// provisioned by td-feed; `td shell` supplies the source-built stage2 Rust and
// native GCC/binutils/glibc recipe outputs as the build platform.
pub fn recipe() -> Recipe {
    Recipe::rust("ripgrep", "14.1.1")
        .source_pin(SourcePin::new(
            "ripgrep-source",
            "https://static.crates.io/crates/ripgrep/ripgrep-14.1.1.crate",
            "f77b8032dc584527975f34aa5a897d0ef5a785573fda778771a614ff9da501d9",
            "ripgrep-14.1.1.crate",
        ))
        .bins(&["rg"])
}
