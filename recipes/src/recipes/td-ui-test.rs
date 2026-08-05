use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

pub fn recipe() -> Recipe {
    let steps = vec![
        Step::Require {
            paths: vec![
                "{in:td-seatd}/bin/td-seatd".into(),
                "{in:td-compositor}/bin/td-compositor".into(),
                "{in:td-compositor}/bin/td-ui-demo".into(),
                "{in:td-compositor}/bin/td-term".into(),
            ],
            exec: true,
        },
        Step::assert_static(&[
            "{in:td-seatd}/bin/td-seatd",
            "{in:td-compositor}/bin/td-compositor",
            "{in:td-compositor}/bin/td-ui-demo",
            "{in:td-compositor}/bin/td-term",
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
        // Reached through the symlink, so the NAME exists and executes in the
        // built artifact. It does not prove dispatch: a td-term that fell
        // through would run the compositor's selftest and exit zero too, and
        // there is no expected-to-FAIL run step to catch that. Dispatch is
        // pinned host-side instead, in the crate's own test and in the
        // recipe's pin on main.rs.
        Step::run("{root}", &["{in:td-compositor}/bin/td-term", "selftest"]),
        Step::MkDir {
            path: "{out}".into(),
        },
        Step::WriteFile {
            path: "{out}/result".into(),
            content: "PASS: td-seatd, td-compositor, td-ui-demo, and td-term are static target executables whose target-side selftests run\n".into(),
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
echo ">> recipe-check td-ui-test: build the dependency-free target seat assigner, software Wayland compositor, demo client, and terminal; assert all are static and execute their target-side selftests"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-ui-test 1
"#,
            )
            .with_runner(CheckRunner::BuildOnly),
        ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The terminal is the newest of the four names this recipe proves, and
    /// every one of these entries is something a build would stay green
    /// without: a missing Require, a missing static assertion, a selftest that
    /// is never run, or a result line claiming more than was checked.
    #[test]
    fn the_terminal_is_proven_beside_the_other_three() {
        let steps = recipe().steps.expect("steps");
        let path = "{in:td-compositor}/bin/td-term";
        assert!(
            steps.iter().any(|step| {
                matches!(step, Step::Require { paths, exec }
                    if *exec && paths.iter().any(|required| required == path))
            }),
            "nothing requires td-term"
        );
        assert!(
            steps.iter().any(|step| {
                matches!(step, Step::AssertStatic { paths }
                    if paths.iter().any(|asserted| asserted == path))
            }),
            "nothing asserts td-term is static"
        );
        assert!(
            steps.iter().any(|step| {
                matches!(step, Step::Run { argv, .. }
                    if argv.first().map(String::as_str) == Some(path)
                        && argv.get(1).map(String::as_str) == Some("selftest"))
            }),
            "nothing runs the terminal's own selftest"
        );
        let claimed = steps.iter().any(|step| {
            matches!(step, Step::WriteFile { content, .. } if content.contains("td-term"))
        });
        assert!(claimed, "the result does not mention what it proved");
    }
}
