use crate::types::Recipe;

/// The sandbox client shares the PTY implementation and the engine's runtime
/// and workload ledger declarations.
pub fn recipe() -> Recipe {
    Recipe::rust("td-sandbox", "0.1.0")
        .local_source("td-sandbox")
        .local_source_trees(&["td-fs", "td-alloc", "td-ui", "td-compositor", "engine"])
        .build_root()
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
        .cargo_subdir("td-sandbox")
        .cargo_lock("td-sandbox/Cargo.lock")
        .static_link()
        .bins(&["td-sandbox"])
}
