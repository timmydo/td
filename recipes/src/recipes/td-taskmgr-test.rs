use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

pub fn recipe() -> Recipe {
    let bin = "{in:td-taskmgr}/bin/td-taskmgr";
    Recipe::mesboot("td-taskmgr-test", "1.0")
        .native_inputs(&["td-taskmgr"])
        .steps(vec![
            Step::Require {
                paths: vec![bin.into()],
                exec: true,
            },
            Step::assert_static(&[bin]),
            Step::run("{root}", &[bin, "--help"]),
            Step::run("{root}", &[bin, "--sample", "2", "--interval", "0.5"]),
            Step::MkDir {
                path: "{out}".into(),
            },
            Step::WriteFile {
                path: "{out}/result".into(),
                content: "PASS: static target td-taskmgr executed help and two bounded resource observations\n".into(),
                exec: false,
            },
            Step::Require {
                paths: vec!["{out}/result".into()],
                exec: false,
            },
        ])
        .checks(vec![RecipeCheck::new(
            "exec \"$TD_RECIPE_EVAL\" check-run td-taskmgr-test 1\n",
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn companion_requires_static_binary_and_executes_both_modes() {
        let recipe = recipe();
        assert_eq!(recipe.native_inputs, Some(vec!["td-taskmgr".into()]));
        let steps = recipe.steps.unwrap();
        let bin = "{in:td-taskmgr}/bin/td-taskmgr";
        let required = steps.iter().position(|s| matches!(s, Step::Require { paths, exec: true } if paths.iter().any(|p| p == bin))).unwrap();
        let static_at = steps
            .iter()
            .position(
                |s| matches!(s, Step::AssertStatic { paths } if paths.iter().any(|p| p == bin)),
            )
            .unwrap();
        let result = steps.iter().position(|s| matches!(s, Step::WriteFile { path, content, .. } if path == "{out}/result" && content.contains("td-taskmgr"))).unwrap();
        for args in [
            vec![bin, "--help"],
            vec![bin, "--sample", "2", "--interval", "0.5"],
        ] {
            let run = steps
                .iter()
                .position(|s| matches!(s, Step::Run { argv, .. } if *argv == args))
                .unwrap();
            assert!(required < static_at && static_at < run && run < result);
        }
    }
}
