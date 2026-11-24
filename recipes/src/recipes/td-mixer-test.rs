use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

/// td-mixer's realized-output check: the static binary prints its usage
/// and the notices td-ui embeds from the staged compositor assets, each
/// proven by exit status alone and neither asking td-audio anything.
/// `--preview` is not run: it writes a binary PPM to standard output,
/// which the step would copy into the build log that `build-run` reads
/// back as UTF-8. The panel's state, its worker's conversation and its
/// headless renders are the crate's own tests on the host preflight; this
/// is target-artifact coverage of the static binary, not of the window.
pub fn recipe() -> Recipe {
    let bin = "{in:td-mixer}/bin/td-mixer";
    Recipe::mesboot("td-mixer-test", "1.0")
        .native_inputs(&["td-mixer"])
        .steps(vec![
            Step::Require {
                paths: vec![bin.into()],
                exec: true,
            },
            Step::assert_static(&[bin]),
            Step::run("{root}", &[bin, "--help"]),
            Step::run("{root}", &[bin, "--font-license"]),
            Step::MkDir {
                path: "{out}".into(),
            },
            Step::WriteFile {
                path: "{out}/result".into(),
                content: "PASS: static target td-mixer printed its usage and font notices\n".into(),
                exec: false,
            },
            Step::Require {
                paths: vec!["{out}/result".into()],
                exec: false,
            },
        ])
        .checks(vec![RecipeCheck::new(
            "exec \"$TD_RECIPE_EVAL\" check-run td-mixer-test 1\n",
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn companion_requires_the_static_binary_and_runs_its_text_modes() {
        let recipe = recipe();
        assert_eq!(recipe.native_inputs, Some(vec!["td-mixer".into()]));
        let steps = recipe.steps.unwrap();
        let bin = "{in:td-mixer}/bin/td-mixer";
        let required = steps
            .iter()
            .position(|s| matches!(s, Step::Require { paths, exec: true } if paths.iter().any(|p| p == bin)))
            .unwrap();
        let static_at = steps
            .iter()
            .position(
                |s| matches!(s, Step::AssertStatic { paths } if paths.iter().any(|p| p == bin)),
            )
            .unwrap();
        let result = steps
            .iter()
            .position(|s| matches!(s, Step::WriteFile { path, content, .. } if path == "{out}/result" && content.contains("td-mixer")))
            .unwrap();
        for args in [vec![bin, "--help"], vec![bin, "--font-license"]] {
            let run = steps
                .iter()
                .position(|s| matches!(s, Step::Run { argv, .. } if *argv == args))
                .unwrap();
            assert!(required < static_at && static_at < run && run < result);
        }
        assert!(!steps
            .iter()
            .any(|step| matches!(step, Step::Run { argv, .. }
            if argv.iter().any(|arg| arg == "--preview"))));
        assert_eq!(recipe.checks.map(|checks| checks.len()), Some(1));
    }
}
