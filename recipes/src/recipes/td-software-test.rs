use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

const TD_SOFTWARE: &str = "{in:td-software}/bin/td-software";

/// The realized-output check for td-software: require the built binary,
/// assert the static shape the system tree demands, then run it on the
/// target in the modes that write text and need neither a display nor a
/// compositor. `--help` proves the entry dispatches; `--font-license`
/// writes the notices td-ui embeds from the staged compositor assets; each
/// is proven by exit status alone, since the sandbox holds no tool to read
/// them back. `--preview` is not run: it writes a binary PPM to standard
/// output, which the step would copy into the build log that `build-run`
/// reads back as UTF-8. The window's state, the answers' parser and the
/// worker's conversation are the crate's own tests on the host preflight;
/// this is target-artifact coverage of the static binary, not of the
/// window.
pub fn recipe() -> Recipe {
    Recipe::mesboot("td-software-test", "1.0")
        .native_inputs(&["td-software"])
        .steps(vec![
            Step::Require {
                paths: vec![TD_SOFTWARE.into()],
                exec: true,
            },
            Step::assert_static(&[TD_SOFTWARE]),
            Step::run("{root}", &[TD_SOFTWARE, "--help"]),
            Step::run("{root}", &[TD_SOFTWARE, "--font-license"]),
            Step::MkDir {
                path: "{out}".into(),
            },
            Step::WriteFile {
                path: "{out}/result".into(),
                content: "PASS: static target td-software executed help and the embedded font licence notices\n"
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
echo ">> recipe-check td-software-test: build the target application manager; assert the binary is static and its text modes run"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-software-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing Require, static assertion or run would leave a build green;
    /// no run may precede the shape it relies on; and no step may write
    /// the binary preview into the build log.
    #[test]
    fn the_manager_is_required_static_and_run() {
        let recipe = recipe();
        assert_eq!(recipe.native_inputs, Some(vec!["td-software".into()]));
        let steps = recipe.steps.expect("steps");
        let at = |what: &str, found: Option<usize>| -> usize {
            found.unwrap_or_else(|| panic!("nothing {what}"))
        };
        let required = at(
            "requires td-software",
            steps.iter().position(|step| {
                matches!(step, Step::Require { paths, exec: true }
                    if paths.iter().any(|path| path == TD_SOFTWARE))
            }),
        );
        let static_at = at(
            "asserts td-software is static",
            steps.iter().position(|step| {
                matches!(step, Step::AssertStatic { paths }
                    if paths.iter().any(|path| path == TD_SOFTWARE))
            }),
        );
        let result = at(
            "writes a result naming what it proved",
            steps.iter().position(|step| {
                matches!(step, Step::WriteFile { path, content, .. }
                    if path == "{out}/result" && content.contains("td-software"))
            }),
        );
        for args in [
            vec![TD_SOFTWARE, "--help"],
            vec![TD_SOFTWARE, "--font-license"],
        ] {
            let run = at(
                "runs a text mode",
                steps
                    .iter()
                    .position(|step| matches!(step, Step::Run { argv, .. } if *argv == args)),
            );
            assert!(required < static_at && static_at < run && run < result);
        }
        assert!(!steps
            .iter()
            .any(|step| matches!(step, Step::Run { argv, .. }
            if argv.iter().any(|arg| arg == "--preview"))));
        assert_eq!(recipe.checks.map(|checks| checks.len()), Some(1));
    }

    /// The modes the check runs are the ones main.rs answers.
    #[test]
    fn the_text_modes_are_the_entry_points_main_dispatches() {
        let main = include_str!("../../../td-software/src/main.rs");
        for arm in ["Some(\"--help\" | \"-h\")", "Some(\"--font-license\")"] {
            assert!(main.contains(arm), "{arm}");
        }
    }
}
