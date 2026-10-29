use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

pub fn recipe() -> Recipe {
    const PROGRAM: &str = "{in:td-sandbox}/bin/td-sandbox";
    Recipe::mesboot("td-sandbox-test", "1.0")
        .native_inputs(&["td-sandbox"])
        .steps(vec![
            Step::Require {
                paths: vec![PROGRAM.into()],
                exec: true,
            },
            Step::assert_static(&[PROGRAM]),
            Step::run("{root}", &[PROGRAM, "--help"]),
            Step::MkDir {
                path: "{out}".into(),
            },
            Step::WriteFile {
                path: "{out}/result".into(),
                content: "PASS: static private-home launcher executes\n".into(),
                exec: false,
            },
        ])
        .checks(vec![RecipeCheck::new(
            "exec \"$TD_RECIPE_EVAL\" check-run td-sandbox-test 1\n",
        )
        .with_runner(CheckRunner::BuildOnly)])
}
