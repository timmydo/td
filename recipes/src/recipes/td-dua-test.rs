use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

const TD_DUA: &str = "{in:td-dua}/bin/td-dua";

/// The realized-output check for td-dua: require the built binary, assert
/// the static shape the system tree demands, then run it on the target in
/// the modes that write text and need no display. `--help` proves the
/// entry dispatches; `--font-license` writes the notices td-ui embeds from
/// the staged compositor assets, proven by exit status alone since the
/// sandbox holds no tool to read them back. `--preview` is not run: it
/// writes a binary PPM to standard output, which the step would copy into
/// the build log that `build-run` reads back as UTF-8, and a frame small
/// enough to keep there paints no tree or treemap. The scan, the window and
/// the delete list are the crate's own tests on the host preflight; this
/// is target-artifact coverage of the static binary, not of the window.
pub fn recipe() -> Recipe {
    Recipe::mesboot("td-dua-test", "1.0")
        .native_inputs(&["td-dua"])
        .steps(vec![
            Step::Require {
                paths: vec![TD_DUA.into()],
                exec: true,
            },
            Step::assert_static(&[TD_DUA]),
            Step::run("{root}", &[TD_DUA, "--help"]),
            Step::run("{root}", &[TD_DUA, "--font-license"]),
            Step::MkDir {
                path: "{out}".into(),
            },
            Step::WriteFile {
                path: "{out}/result".into(),
                content: "PASS: static target td-dua executed help and the embedded font licence notices\n"
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
echo ">> recipe-check td-dua-test: build the target disk usage analyzer; assert the binary is static and its text modes run"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-dua-test 1
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
    fn the_analyzer_is_required_static_and_run() {
        let recipe = recipe();
        assert_eq!(recipe.native_inputs, Some(vec!["td-dua".into()]));
        let steps = recipe.steps.expect("steps");
        let at = |what: &str, found: Option<usize>| -> usize {
            found.unwrap_or_else(|| panic!("nothing {what}"))
        };
        let required = at(
            "requires td-dua",
            steps.iter().position(|step| {
                matches!(step, Step::Require { paths, exec: true }
                    if paths.iter().any(|path| path == TD_DUA))
            }),
        );
        let static_at = at(
            "asserts td-dua is static",
            steps.iter().position(|step| {
                matches!(step, Step::AssertStatic { paths }
                    if paths.iter().any(|path| path == TD_DUA))
            }),
        );
        let result = at(
            "writes a result naming what it proved",
            steps.iter().position(|step| {
                matches!(step, Step::WriteFile { path, content, .. }
                    if path == "{out}/result" && content.contains("td-dua"))
            }),
        );
        for args in [vec![TD_DUA, "--help"], vec![TD_DUA, "--font-license"]] {
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
        let main = include_str!("../../../td-dua/src/main.rs");
        for arm in ["Some(\"--help\" | \"-h\")", "Some(\"--font-license\")"] {
            assert!(main.contains(arm), "{arm}");
        }
    }
}
