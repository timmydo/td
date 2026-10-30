use crate::types::Recipe;

pub fn recipe() -> Recipe {
    Recipe::rust("td-time", "0.1.0")
        .local_source("td-time")
        .local_source_trees(&["td-civil", "td-fs", "engine"])
        .build_root()
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
        .cargo_subdir("td-time")
        .cargo_lock("td-time/Cargo.lock")
        .static_link()
        .bins(&["td-time"])
}
