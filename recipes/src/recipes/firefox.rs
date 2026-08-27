use crate::types::{Recipe, Step};

/// The exact Flathub Firefox deploy, still only a package tree at this rung.
/// Application metadata and the dynamic-runtime validator land together in the
/// next increment so an unvalidated dynamic package is never launchable.
pub fn recipe() -> Recipe {
    Recipe::mesboot("firefox", "154.0")
        .source_input("firefox-154-source")
        .steps(vec![Step::CopyTree {
            from: "{payload:firefox-source}".into(),
            dest: "{out}/files".into(),
        }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deploy_is_copied_only_through_the_typed_data_name() {
        let recipe = recipe();
        assert!(recipe.is_foreign());
        assert!(recipe.is_foreign_source());
        assert_eq!(recipe.source_input.as_deref(), Some("firefox-154-source"));
        assert!(matches!(
            recipe.steps.as_deref(),
            Some([Step::CopyTree { from, dest }])
                if from == "{payload:firefox-source}" && dest == "{out}/files"
        ));
    }
}
