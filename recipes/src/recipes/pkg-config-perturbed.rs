use crate::types::Recipe;

// Self-discrimination twin (see hello-perturbed.rs): same source + CFLAGS, one
// deliberate configureFlags delta (--with → --without-internal-glib), so it assembles
// a DISTINCT .drv — proving the recipe's configureFlags are load-bearing in the build.
pub fn recipe() -> Recipe {
    super::pkg_config::recipe()
        .configure_flags(&["--without-internal-glib", "CFLAGS=-O2 -g -std=gnu17"])
}
