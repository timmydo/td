//! The package catalog — every td recipe, declared in Rust.
//!
//! Keyed by a stable STEM (not the recipe name): the `-perturbed`
//! self-discrimination twins deliberately share a recipe `name` with their base
//! (e.g. `hello-perturbed` is name `hello`), so the stem is the stable key. The
//! `recipe-rs` gate proves the surface is self-consistent.
//!
//! Each recipe lives in its own self-registering file `src/recipes/<stem>.rs`
//! (github issue #295): the file name IS the stem, `pub fn recipe() -> Recipe`
//! is the registration, and `build.rs` generates the stem-sorted registry
//! (module declarations + the `all()` table) included below. Adding a recipe
//! touches only its new file: no Rust source line is shared, so parallel recipe
//! PRs don't collide on a central table (the mk/gates/ one-file-per-entry property).

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
    fn catalog_is_sorted_and_stems_are_unique() {
        // The generated registry must stay stem-sorted (the stable `list`
        // order) with no duplicate stems, whatever read_dir order build.rs saw.
        let stems: Vec<&str> = all().into_iter().map(|(s, _)| s).collect();
        let mut sorted = stems.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(stems, sorted, "catalog stems are not sorted+unique");
    }

    #[test]
    fn first_seed_binds_one_foreign_pin_to_one_payload_runtime() {
        let seed = lookup("ripgrep-seed").expect("ripgrep seed recipe");
        assert!(seed.is_foreign(), "the prebuilt source pin must mark its recipe");
        assert!(seed.is_foreign_source());
        assert_eq!(seed.name, "ripgrep-seed");
        assert_eq!(seed.version, "15.2.0");
        assert_eq!(seed.source_input.as_deref(), Some("ripgrep-seed-source"));
        assert_eq!(seed.payload_inputs, Some(vec!["empty-runtime".to_string()]));
        let declaration = seed.application.as_ref().expect("application declaration");
        assert_eq!(declaration.runtime(), "empty-runtime");
        assert_eq!(declaration.entry(), "/app/bin/rg");
        assert!(seed.steps.as_ref().is_some_and(|steps| {
            steps.last().is_some_and(|step| {
                matches!(
                    step,
                    crate::types::Step::ValidateStaticApplication { entry, runtime }
                        if entry == declaration.entry() && runtime == declaration.runtime()
                )
            }) && steps.iter().any(|step| {
                matches!(
                    step,
                    crate::types::Step::Unpack { input, .. }
                        if input == "{payload:ripgrep-seed-source}"
                )
            })
        }));

        let runtime = lookup("empty-runtime").expect("empty runtime recipe");
        assert!(!runtime.is_foreign());
        assert!(runtime.application.is_none());
        assert!(runtime.source_input.is_none());
    }
}
