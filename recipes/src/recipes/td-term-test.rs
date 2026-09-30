use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

const TD_TERM: &str = "{in:td-term}/bin/td-term";
const ENTRY: &str = "{in:td-term-terminfo}/share/terminfo/t/td-term";

/// The realized-output check for td-term: require the built binary, assert
/// the static shape the system tree demands, run its target-side selftest,
/// and require the compiled terminfo entry the image points `/etc/terminfo`
/// at. The selftest composes the terminal model, keyboard encoder,
/// renderer, terminfo compiler, PTY mechanism, session policy, readiness
/// codec and client, and prints its own marker only once every layer ran.
/// A window is the crate's own tests under the native harness on the host
/// preflight and the boot oracle in the image; this is target-artifact
/// coverage of the static binary.
pub fn recipe() -> Recipe {
    Recipe::mesboot("td-term-test", "1.0")
        .native_inputs(&["td-term", "td-term-terminfo"])
        .steps(vec![
            Step::Require {
                paths: vec![TD_TERM.into()],
                exec: true,
            },
            Step::assert_static(&[TD_TERM]),
            Step::run("{root}", &[TD_TERM, "selftest"]),
            Step::Require {
                paths: vec![ENTRY.into()],
                exec: false,
            },
            Step::MkDir {
                path: "{out}".into(),
            },
            Step::WriteFile {
                path: "{out}/result".into(),
                content: "PASS: td-term is a static target executable whose target-side selftest runs, and its compiled terminfo entry is present\n".into(),
                exec: false,
            },
            Step::Require {
                paths: vec!["{out}/result".into()],
                exec: false,
            },
        ])
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-term-test: build the dependency-free target terminal and its terminfo entry; assert the binary is static and executes its target-side selftest"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-term-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every entry here is something a build would stay green without: a
    /// missing Require, a missing static assertion, a selftest never run, or
    /// a result line claiming more than was checked; and a run cannot
    /// precede the shape it relies on.
    #[test]
    fn the_terminal_is_required_static_and_selftested() {
        let recipe = recipe();
        let steps = recipe.steps.expect("steps");
        let position = |found: Option<usize>, what: &str| -> usize {
            found.unwrap_or_else(|| panic!("nothing {what}"))
        };
        let required = position(
            steps.iter().position(|step| {
                matches!(step, Step::Require { paths, exec: true }
                    if paths.iter().any(|path| path == TD_TERM))
            }),
            "requires td-term",
        );
        let static_at = position(
            steps.iter().position(|step| {
                matches!(step, Step::AssertStatic { paths }
                    if paths.iter().any(|path| path == TD_TERM))
            }),
            "asserts td-term is static",
        );
        let selftest = position(
            steps.iter().position(
                |step| matches!(step, Step::Run { argv, .. } if *argv == [TD_TERM, "selftest"]),
            ),
            "runs the terminal's own selftest",
        );
        let entry = position(
            steps.iter().position(|step| {
                matches!(step, Step::Require { paths, exec: false }
                    if paths.iter().any(|path| path == ENTRY))
            }),
            "requires the terminfo entry",
        );
        let result = position(
            steps.iter().position(|step| {
                matches!(step, Step::WriteFile { path, content, .. }
                    if path == "{out}/result" && content.contains("td-term")
                        && content.contains("terminfo"))
            }),
            "writes a result naming what it proved",
        );
        assert!(required < static_at && static_at < selftest && selftest < result);
        assert!(entry < result);
        assert_eq!(
            recipe.checks.map(|checks| checks.len()),
            Some(1),
            "one build-only check"
        );
    }

    /// The subcommand the check runs and the marker it relies on are the
    /// ones main.rs dispatches and prints.
    #[test]
    fn the_selftest_is_the_one_main_dispatches() {
        let main = include_str!("../../../td-term/src/main.rs");
        assert!(main.contains("\"selftest\" =>"));
        assert!(main.contains("TD-TERM-SELFTEST-OK"));
    }
}
