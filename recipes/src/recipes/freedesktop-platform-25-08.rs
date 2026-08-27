use crate::types::{Recipe, Step};

/// The exact Freedesktop 25.08 runtime deploy. It is a marked payload recipe,
/// not a compiler/runtime input to any source-built package.
pub fn recipe() -> Recipe {
    Recipe::mesboot("freedesktop-platform-25-08", "25.08")
        .source_input("freedesktop-platform-25-08-source")
        .steps(vec![Step::CopyTree {
            from: "{payload:freedesktop-platform-25-08-source}".into(),
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
        assert_eq!(
            recipe.source_input.as_deref(),
            Some("freedesktop-platform-25-08-source")
        );
        assert!(matches!(
            recipe.steps.as_deref(),
            Some([Step::CopyTree { from, dest }])
                if from == "{payload:freedesktop-platform-25-08-source}"
                    && dest == "{out}/files"
        ));
    }
}
