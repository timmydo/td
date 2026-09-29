use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

/// Check the realized target binary before it is admitted to a live image.
/// The host native-compositor suite covers the actual welcome window; here
/// the target tool must link statically, render every page and run its text modes.
pub fn recipe() -> Recipe {
    let bin = "{in:td-setup}/bin/td-setup";
    Recipe::mesboot("td-setup-test", "1.0")
        .native_inputs(&["td-setup"])
        .steps(vec![
            Step::Require {
                paths: vec![bin.into()],
                exec: true,
            },
            Step::assert_static(&[bin]),
            Step::run("{root}", &[bin, "--help"]),
            Step::run("{root}", &[bin, "--font-license"]),
            Step::run("{root}", &[bin, "--render-check"]),
            Step::MkDir {
                path: "{out}".into(),
            },
            Step::WriteFile {
                path: "{out}/result".into(),
                content:
                    "PASS: source-built static td-setup rendered all pages and executed text modes\n"
                        .into(),
                exec: false,
            },
            Step::Require {
                paths: vec!["{out}/result".into()],
                exec: false,
            },
        ])
        .checks(vec![RecipeCheck::new(
            "exec \"$TD_RECIPE_EVAL\" check-run td-setup-test 1\n",
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_check_proves_static_shape_before_running_the_headless_modes() {
        let recipe = recipe();
        assert_eq!(recipe.native_inputs, Some(vec!["td-setup".into()]));
        let steps = recipe.steps.unwrap();
        let bin = "{in:td-setup}/bin/td-setup";
        let required = steps
            .iter()
            .position(|step| matches!(step, Step::Require { paths, exec: true } if paths.iter().any(|path| path == bin)))
            .unwrap();
        let static_check = steps
            .iter()
            .position(|step| matches!(step, Step::AssertStatic { paths } if paths.iter().any(|path| path == bin)))
            .unwrap();
        let result = steps
            .iter()
            .position(|step| matches!(step, Step::WriteFile { path, content, .. } if path == "{out}/result" && content.contains("rendered all pages")))
            .unwrap();
        for flag in ["--help", "--font-license", "--render-check"] {
            let run = steps
                .iter()
                .position(|step| matches!(step, Step::Run { argv, .. } if argv == &vec![bin.to_string(), flag.to_string()]))
                .unwrap();
            assert!(required < static_check && static_check < run && run < result);
            let main = include_str!("../../../td-setup/src/main.rs");
            assert!(main.contains(&format!("[arg] if arg == \"{flag}\"")));
        }
    }
}
