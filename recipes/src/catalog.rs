//! The package catalog — every td recipe, declared in Rust.
//!
//! Keyed by a stable STEM (not the recipe name): the `-perturbed`
//! self-discrimination twins deliberately share a recipe `name` with their base
//! (e.g. `hello-perturbed` is name `hello`), so the stem is the stable key. The
//! `recipe-rs` gate proves the surface is self-consistent and keeps the
//! `tests/recipes-meta.json` recipe manifest in sync.
//!
//! Each recipe lives in its own self-registering file `src/recipes/<stem>.rs`
//! (github issue #295): the file name IS the stem, `pub fn recipe() -> Recipe`
//! is the registration, and `build.rs` generates the stem-sorted registry
//! (module declarations + the `all()` table) included below. Adding a recipe
//! touches only its new file plus the shared regenerate-on-rebase recipe
//! manifest (`tests/recipes-meta.json` — issue #296): no
//! Rust source line is shared, so parallel recipe PRs don't collide on a
//! central table (the mk/gates/ one-file-per-entry property).

use crate::types::Recipe;

/// Look up a recipe by `.ts` file stem (e.g. "hello", "gzip-perturbed").
pub fn lookup(stem: &str) -> Option<Recipe> {
    all().into_iter().find(|(s, _)| *s == stem).map(|(_, r)| r)
}

/// Every migrated recipe, paired with its `.ts` file stem, sorted by stem.
pub fn all() -> Vec<(&'static str, Recipe)> {
    registry::all()
}

mod registry {
    include!(concat!(env!("OUT_DIR"), "/registry.rs"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_recipe_emits_canonical_json_and_round_trips() {
        for (stem, r) in all() {
            let canon = r.to_json().to_canonical();
            // Structural self-consistency: re-parsing the emitted JSON and
            // re-canonicalising yields the same bytes (the durable round-trip).
            let reparsed = crate::json::parse(&canon)
                .unwrap_or_else(|e| panic!("{stem}: emitted invalid JSON: {e}"));
            assert_eq!(reparsed.to_canonical(), canon, "{stem}: not idempotent");
            assert!(!r.name.is_empty() && !r.version.is_empty(), "{stem}: missing fields");
        }
    }

    #[test]
    fn perturbed_twins_diverge_from_their_base() {
        // The self-discrimination property the corpus gates rely on: a perturbed
        // twin must NOT serialise identically to its base.
        let pairs = [
            ("hello", "hello-perturbed"),
            ("pkg-config", "pkg-config-perturbed"),
        ];
        for (base, pert) in pairs {
            let b = lookup(base).unwrap().to_json().to_canonical();
            let p = lookup(pert).unwrap().to_json().to_canonical();
            assert_ne!(b, p, "{pert} did not diverge from {base}");
        }
    }

    #[test]
    fn catalog_is_sorted_and_stems_are_unique() {
        // The generated registry must stay stem-sorted (the stable `list`/`meta`
        // order) with no duplicate stems, whatever read_dir order build.rs saw.
        let stems: Vec<&str> = all().into_iter().map(|(s, _)| s).collect();
        let mut sorted = stems.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(stems, sorted, "catalog stems are not sorted+unique");
    }
}
