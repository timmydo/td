use crate::types::Recipe;

// Self-discrimination twin (see hello-perturbed.rs): same source, one deliberate
// configureFlags delta.
pub fn recipe() -> Recipe {
    super::pkg_config::recipe().configure_flags(&["--without-internal-glib"])
}
