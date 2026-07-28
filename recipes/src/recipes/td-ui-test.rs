use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

pub fn recipe() -> Recipe {
    let steps = vec![
        Step::Require {
            paths: vec![
                "{in:td-seatd}/bin/td-seatd".into(),
                "{in:td-compositor}/bin/td-compositor".into(),
                "{in:td-compositor}/bin/td-ui-demo".into(),
            ],
            exec: true,
        },
        Step::assert_static(&[
            "{in:td-seatd}/bin/td-seatd",
            "{in:td-compositor}/bin/td-compositor",
            "{in:td-compositor}/bin/td-ui-demo",
        ]),
        Step::run("{root}", &["{in:td-seatd}/bin/td-seatd", "selftest"]),
        Step::run(
            "{root}",
            &["{in:td-compositor}/bin/td-compositor", "selftest"],
        ),
        Step::run(
            "{root}",
            &["{in:td-compositor}/bin/td-ui-demo", "selftest"],
        ),
        Step::MkDir {
            path: "{out}".into(),
        },
        Step::WriteFile {
            path: "{out}/result".into(),
            content: "PASS: td-seatd, td-compositor, and td-ui-demo are static target executables whose target-side selftests run\n".into(),
            exec: false,
        },
        Step::Require {
            paths: vec!["{out}/result".into()],
            exec: false,
        },
    ];

    Recipe::mesboot("td-ui-test", "1.0")
        .native_inputs(&["td-seatd", "td-compositor"])
        .steps(steps)
        .checks(vec![
            RecipeCheck::new(
                r#"
echo ">> recipe-check td-ui-test: build the dependency-free target seat assigner, software Wayland compositor, and demo client; assert all are static and execute their target-side selftests"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-ui-test 1
"#,
            )
            .with_runner(CheckRunner::BuildOnly),
        ])
}
