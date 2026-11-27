use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

const TD_DESKTOP: &str = "{in:td-desktop}/bin/td-desktop";

pub fn recipe() -> Recipe {
    Recipe::mesboot("td-desktop-test", "1.0")
        .native_inputs(&["td-desktop"])
        .steps(vec![
            Step::Require {
                paths: vec![TD_DESKTOP.into()],
                exec: true,
            },
            Step::assert_static(&[TD_DESKTOP]),
            Step::run("{root}", &[TD_DESKTOP, "--help"]),
            Step::MkDir {
                path: "{out}".into(),
            },
            Step::WriteFile {
                path: "{out}/result".into(),
                content: "PASS: static target td-desktop executed help\n".into(),
                exec: false,
            },
            Step::Require {
                paths: vec!["{out}/result".into()],
                exec: false,
            },
        ])
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-desktop-test: build and run the static desktop viewer"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-desktop-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}
