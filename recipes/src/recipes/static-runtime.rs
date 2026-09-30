use super::jetbrains_mono_nerd_font::DIR as FONT_DIR;
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

/// Data for source-built static applications; no loader or executable ABI.
/// The outline face sits under `files/etc/fonts`, a real directory td-jail
/// binds at `/etc/fonts`, so a jailed td-ui program reads it at the name the
/// image's `/etc/fonts/jetbrains-mono-nerd` gives an unjailed one.
pub fn recipe() -> Recipe {
    Recipe::mesboot("static-runtime", "1")
        .inputs(&["tzdata", "jetbrains-mono-nerd-font"])
        .steps(vec![
            Step::CopyTree {
                from: "{in:tzdata}/share/zoneinfo".into(),
                dest: "{out}/files/share/zoneinfo".into(),
            },
            Step::CopyTree {
                from: "{in:tzdata}/share/doc/tzdata".into(),
                dest: "{out}/files/share/doc/tzdata".into(),
            },
            Step::CopyTree {
                from: format!("{{in:jetbrains-mono-nerd-font}}/{FONT_DIR}"),
                dest: "{out}/files/etc/fonts/jetbrains-mono-nerd".into(),
            },
            Step::Require {
                paths: vec![
                    "{out}/files/share/zoneinfo/Etc/UTC".into(),
                    "{out}/files/share/zoneinfo/Europe/London".into(),
                    "{out}/files/share/doc/tzdata/LICENSE".into(),
                    "{out}/files/etc/fonts/jetbrains-mono-nerd/JetBrainsMonoNerdFontMono-Regular.ttf"
                        .into(),
                    "{out}/files/etc/fonts/jetbrains-mono-nerd/OFL.txt".into(),
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
    fn static_runtime_contains_only_timezone_data_and_the_outline_face() {
        let recipe = recipe();
        assert!(!recipe.is_foreign());
        assert_eq!(
            recipe.inputs,
            Some(vec!["tzdata".into(), "jetbrains-mono-nerd-font".into()])
        );
        let steps = recipe.steps.as_deref().unwrap_or_default();
        assert!(
            matches!(steps, [Step::CopyTree { from: zones, dest: zone_dest },
            Step::CopyTree { from: docs, dest: doc_dest },
            Step::CopyTree { from: face, dest: face_dest }, Step::Require { exec: false, .. }]
            if zones == "{in:tzdata}/share/zoneinfo"
                && zone_dest == "{out}/files/share/zoneinfo"
                && docs == "{in:tzdata}/share/doc/tzdata"
                && doc_dest == "{out}/files/share/doc/tzdata"
                && face == "{in:jetbrains-mono-nerd-font}/share/fonts/jetbrains-mono-nerd"
                && face_dest == "{out}/files/etc/fonts/jetbrains-mono-nerd")
        );
    }
}
