use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

pub fn recipe() -> Recipe {
    let steps = vec![
        Step::Require {
            paths: vec![
                "{in:td-seatd}/bin/td-seatd".into(),
                "{in:td-compositor}/bin/td-compositor".into(),
                "{in:td-compositor}/bin/td-ui-demo".into(),
                "{in:td-compositor}/bin/td-term".into(),
                "{in:td-compositor}/bin/td-ctl".into(),
            ],
            exec: true,
        },
        Step::assert_static(&[
            "{in:td-seatd}/bin/td-seatd",
            "{in:td-compositor}/bin/td-compositor",
            "{in:td-compositor}/bin/td-ui-demo",
            "{in:td-compositor}/bin/td-term",
            "{in:td-compositor}/bin/td-ctl",
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
        // This one DOES prove dispatch, which is why it is `help` and not a
        // selftest: `help` is not a compositor subcommand, so a td-ctl that
        // fell through to the default personality would exit non-zero on this
        // exact argv and fail the build. It needs no session — the vocabulary
        // is compiled in — so it proves the fourth name is a program without
        // standing up a compositor to answer it.
        Step::run("{root}", &["{in:td-compositor}/bin/td-ctl", "help"]),
        Step::MkDir {
            path: "{out}".into(),
        },
        Step::WriteFile {
            path: "{out}/result".into(),
            content: "PASS: td-seatd, td-compositor, td-ui-demo, td-term, and td-ctl are static target executables whose target-side selftests run, and td-ctl's own argv[0] dispatch is proven by a subcommand no other personality takes\n".into(),
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

    /// Every one of these entries is something a build would stay green
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

    /// The control client is the newest name, and the only one whose run step
    /// proves DISPATCH rather than mere existence: `help` is not a compositor
    /// subcommand, so a binary that fell through would exit non-zero here.
    /// That property is what this pins — a step changed to `selftest` would
    /// still pass on the target and would prove one thing less.
    #[test]
    fn the_control_client_is_proven_by_an_argv_no_other_personality_takes() {
        let steps = recipe().steps.expect("steps");
        let path = "{in:td-compositor}/bin/td-ctl";
        assert!(
            steps.iter().any(|step| {
                matches!(step, Step::Require { paths, exec }
                    if *exec && paths.iter().any(|required| required == path))
            }),
            "nothing requires td-ctl"
        );
        assert!(
            steps.iter().any(|step| {
                matches!(step, Step::AssertStatic { paths }
                    if paths.iter().any(|asserted| asserted == path))
            }),
            "nothing asserts td-ctl is static"
        );
        assert!(
            steps.iter().any(|step| {
                matches!(step, Step::Run { argv, .. }
                    if argv.first().map(String::as_str) == Some(path)
                        && argv.get(1).map(String::as_str) == Some("help"))
            }),
            "nothing runs td-ctl with an argv only the control personality takes"
        );
        assert!(
            steps.iter().any(|step| {
                matches!(step, Step::WriteFile { content, .. } if content.contains("td-ctl"))
            }),
            "the result does not mention the control client"
        );
    }
}
