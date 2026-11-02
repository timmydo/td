use crate::types::Recipe;

pub fn recipe() -> Recipe {
    Recipe::rust("td-disks", "0.1.0")
        .local_source("td-disks")
        .local_source_trees(&["td-btrfs", "td-fs"])
        .build_root()
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
        .cargo_subdir("td-disks")
        .cargo_lock("td-disks/Cargo.lock")
        .static_link()
        .bins(&["td-disks"])
}
