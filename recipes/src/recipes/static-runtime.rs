use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

/// Data for source-built static applications; no loader or executable ABI.
pub fn recipe() -> Recipe {
    Recipe::mesboot("static-runtime", "1")
        .inputs(&["tzdata"])
        .steps(vec![
            Step::CopyTree {
                from: "{in:tzdata}/share/zoneinfo".into(),
                dest: "{out}/files/share/zoneinfo".into(),
            },
            Step::CopyTree {
                from: "{in:tzdata}/share/doc/tzdata".into(),
                dest: "{out}/files/share/doc/tzdata".into(),
            },
            Step::Require {
                paths: vec![
                    "{out}/files/share/zoneinfo/Etc/UTC".into(),
                    "{out}/files/share/zoneinfo/Europe/London".into(),
                    "{out}/files/share/doc/tzdata/LICENSE".into(),
                ],
                exec: false,
            },
        ])
        .checks(vec![RecipeCheck::new(
            "exec \"${TD_RECIPE_EVAL:-$PWD/target/release/td-recipe-eval}\" check-run static-runtime 1\n",
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_runtime_contains_only_declared_source_built_timezone_data() {
        let recipe = recipe();
        assert!(!recipe.is_foreign());
        assert_eq!(recipe.inputs, Some(vec!["tzdata".into()]));
        let steps = recipe.steps.as_deref().unwrap_or_default();
        assert!(
            matches!(steps, [Step::CopyTree { from: zones, dest: zone_dest },
            Step::CopyTree { from: docs, dest: doc_dest }, Step::Require { exec: false, .. }]
            if zones == "{in:tzdata}/share/zoneinfo"
                && zone_dest == "{out}/files/share/zoneinfo"
                && docs == "{in:tzdata}/share/doc/tzdata"
                && doc_dest == "{out}/files/share/doc/tzdata")
        );
    }
}
