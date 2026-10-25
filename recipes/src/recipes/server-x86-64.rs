use crate::types::Recipe;

/// The headless td system: system-x86-64's boot path, primary account,
/// userland and OpenSSH under td-svc, with nothing graphical packed and no
/// console login (td-svc/DESIGN.md §10, "Profiles"). Its definition is
/// `SERVER` in system-x86-64.rs, so the two profiles cannot drift apart.
pub fn recipe() -> Recipe {
    super::system_x86_64::server_recipe()
}
