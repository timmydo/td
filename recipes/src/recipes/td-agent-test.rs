use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

const BIN: &str = "{in:td-agent}/bin/td-agent";

/// td-agent's realized-output check: the binary exists, is static, and
/// answers its three help modes, which read no state, configuration or key.
/// The window needs a compositor and the model a network, which a build
/// sandbox has neither of.
pub fn recipe() -> Recipe {
    let steps = vec![
        Step::Require {
            paths: vec![BIN.into()],
            exec: true,
        },
        Step::assert_static(&[BIN]),
        Step::run("{root}", &[BIN, "--help"]),
        Step::run("{root}", &[BIN, "review", "--help"]),
        Step::run("{root}", &[BIN, "calibrate", "--help"]),
        Step::MkDir {
            path: "{out}".into(),
        },
        Step::WriteFile {
            path: "{out}/result".into(),
            content: "PASS: static target td-agent answered its window's, review's and calibrate's help\n"
                .into(),
            exec: false,
        },
        Step::Require {
            paths: vec!["{out}/result".into()],
            exec: false,
        },
    ];
    Recipe::mesboot("td-agent-test", "1.0")
        .native_inputs(&["td-agent", "glibc-x86-64"])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            "exec \"$TD_RECIPE_EVAL\" check-run td-agent-test 1\n",
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(steps: &[Step], want: &[&str]) -> usize {
        steps
            .iter()
            .position(|s| matches!(s, Step::Run { argv, .. } if *argv == want))
            .unwrap_or_else(|| panic!("no step runs {want:?}"))
    }

    #[test]
    fn the_static_binary_answers_each_help() {
        let recipe = recipe();
        assert_eq!(
            recipe.native_inputs,
            Some(vec!["td-agent".into(), "glibc-x86-64".into()])
        );
        let steps = recipe.steps.unwrap();
        let static_at = steps
            .iter()
            .position(
                |s| matches!(s, Step::AssertStatic { paths } if paths.iter().any(|p| p == BIN)),
            )
            .unwrap();
        let help = position(&steps, &[BIN, "--help"]);
        let review = position(&steps, &[BIN, "review", "--help"]);
        let calibrate = position(&steps, &[BIN, "calibrate", "--help"]);
        let result = steps
            .iter()
            .position(|s| matches!(s, Step::WriteFile { path, .. } if path == "{out}/result"))
            .unwrap();
        assert!(static_at < help && help < review && review < calibrate && calibrate < result);
    }
}
