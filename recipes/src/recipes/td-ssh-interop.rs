use crate::types::Recipe;

/// The driver of td-ssh-test's real logins (td-ssh-interop/src/main.rs): a
/// std-only program that nothing but that check runs, and no image holds. It
/// mounts td-firstboot's `ssh_policy.rs` by relative `#[path]` to hand every form
/// of the image's policy to the real td-sshd, so td-firstboot is staged beside it.
pub fn recipe() -> Recipe {
    Recipe::rust("td-ssh-interop", "0.1.0")
        .local_source("td-ssh-interop")
        .local_source_trees(&["td-firstboot"])
        .build_root()
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
        .cargo_subdir("td-ssh-interop")
        .cargo_lock("td-ssh-interop/Cargo.lock")
        .static_link()
        .bins(&["td-ssh-interop"])
}
