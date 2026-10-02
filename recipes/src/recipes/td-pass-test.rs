use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

const TD_PASS: &str = "{in:td-pass}/bin/td-pass";

/// The realized-output check for td-pass: require the built binary, assert
/// the static shape the system tree demands, and run it on the target.
/// `--help` proves the entry dispatches past td-secret's worker check
/// without a display, a vault or a token. The window and the notebook are
/// the crate's own tests, its fixture backend under the native compositor
/// harness on the host preflight; a real vault needs a FIDO2 key and is
/// hardware evidence. This is target-artifact coverage of the static
/// binary.
pub fn recipe() -> Recipe {
    Recipe::mesboot("td-pass-test", "1.0")
        .native_inputs(&["td-pass"])
        .steps(vec![
            Step::Require {
                paths: vec![TD_PASS.into()],
                exec: true,
            },
            Step::assert_static(&[TD_PASS]),
            Step::run("{root}", &[TD_PASS, "--help"]),
            Step::MkDir {
                path: "{out}".into(),
            },
            Step::WriteFile {
                path: "{out}/result".into(),
                content: "PASS: td-pass is a static target executable whose entry dispatches --help\n"
                    .into(),
                exec: false,
            },
            Step::Require {
                paths: vec!["{out}/result".into()],
                exec: false,
            },
        ])
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-pass-test: build the target notebook; assert the binary is static and its entry dispatches"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-pass-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing Require, static assertion or run would leave a build green;
    /// and no run may precede the shape it relies on.
    #[test]
    fn the_notebook_is_required_static_and_run() {
        let recipe = recipe();
        let steps = recipe.steps.expect("steps");
        let at = |what: &str, found: Option<usize>| -> usize {
            found.unwrap_or_else(|| panic!("nothing {what}"))
        };
        let required = at(
            "requires td-pass",
            steps.iter().position(|step| {
                matches!(step, Step::Require { paths, exec: true }
                    if paths.iter().any(|path| path == TD_PASS))
            }),
        );
        let static_at = at(
            "asserts td-pass is static",
            steps.iter().position(|step| {
                matches!(step, Step::AssertStatic { paths }
                    if paths.iter().any(|path| path == TD_PASS))
            }),
        );
        let run = at(
            "runs td-pass --help",
            steps.iter().position(
                |step| matches!(step, Step::Run { argv, .. } if *argv == [TD_PASS, "--help"]),
            ),
        );
        let result = at(
            "writes a result naming what it proved",
            steps.iter().position(|step| {
                matches!(step, Step::WriteFile { path, content, .. }
                    if path == "{out}/result" && content.contains("td-pass"))
            }),
        );
        assert!(required < static_at && static_at < run && run < result);
        assert_eq!(recipe.checks.map(|checks| checks.len()), Some(1));
    }

    /// The flag the check runs is the one main.rs answers.
    #[test]
    fn help_is_the_one_main_answers() {
        let main = include_str!("../../../td-pass/src/main.rs");
        assert!(main.contains("if args == [\"--help\"]"));
    }
}
