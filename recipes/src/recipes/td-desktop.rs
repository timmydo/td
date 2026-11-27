use crate::types::Recipe;

/// The viewer and the fixed SSH session helper are entry points in one
/// static binary. td-ui shares wire and font sources with the compositor.
pub fn recipe() -> Recipe {
    Recipe::rust("td-desktop", "0.1.0")
        .local_source("td-desktop")
        .local_source_trees(&[
            "td-alloc",
            "td-desktop-protocol",
            "td-fs",
            "td-remote",
            "td-ui",
            "td-compositor",
        ])
        .build_root()
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
        .cargo_subdir("td-desktop")
        .cargo_lock("td-desktop/Cargo.lock")
        .static_link()
        .bins(&["td-desktop"])
}
