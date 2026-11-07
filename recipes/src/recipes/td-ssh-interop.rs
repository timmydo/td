use crate::types::Recipe;

/// The driver of td-ssh-test's real logins (td-ssh-interop/src/main.rs): a
/// std-only program that nothing but that check runs, and no image holds.
pub fn recipe() -> Recipe {
    Recipe::rust("td-ssh-interop", "0.1.0")
        .local_source("td-ssh-interop")
        .build_root()
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
        .cargo_lock("td-ssh-interop/Cargo.lock")
        .static_link()
        .bins(&["td-ssh-interop"])
}
