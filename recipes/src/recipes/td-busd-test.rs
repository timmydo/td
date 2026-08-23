use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

pub fn recipe() -> Recipe {
    let bin = "{in:td-busd}/bin/td-busd";
    let steps = vec![
        Step::Require {
            paths: vec![bin.into()],
            exec: true,
        },
        Step::assert_static(&[bin]),
        // The host tests run the same corpus, so what this adds is the TARGET
        // build of it: the same committed byte streams decoded by a binary
        // rustc built for x86-64 with `panic=abort` and `opt-level=s`, where a
        // refusal this crate returns as an error and a refusal it reaches by
        // panicking are no longer the same observation.
        Step::run("{root}", &[bin, "selftest"]),
        Step::MkDir {
            path: "{out}".into(),
        },
        Step::WriteFile {
            path: "{out}/result".into(),
            content: "PASS: td-busd is a static target executable whose D-Bus selftest runs \
there — every type round-tripped in both byte orders, the committed body and message streams \
marshalled and decoded byte for byte, every committed malformed encoding refused, and every \
committed auth transcript answered byte for byte\n"
                .into(),
            exec: false,
        },
        Step::Require {
            paths: vec!["{out}/result".into()],
            exec: false,
        },
    ];

    Recipe::mesboot("td-busd-test", "1.0")
        .native_inputs(&["td-busd"])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-busd-test: build the dependency-free target D-Bus broker, assert it is static, and run its codec and handshake selftest on the target build"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-busd-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every one of these is something a build would stay green without: a
    /// missing Require, a missing static assertion, or a selftest that is
    /// never run.
    #[test]
    fn the_broker_is_required_static_and_actually_run() {
        let steps = recipe().steps.expect("steps");
        let bin = "{in:td-busd}/bin/td-busd";
        assert!(
            steps.iter().any(|step| {
                matches!(step, Step::Require { paths, exec }
                    if *exec && paths.iter().any(|required| required == bin))
            }),
            "nothing requires td-busd"
        );
        assert!(
            steps.iter().any(|step| {
                matches!(step, Step::AssertStatic { paths }
                    if paths.iter().any(|asserted| asserted == bin))
            }),
            "nothing asserts td-busd is static"
        );
        assert!(
            steps.iter().any(|step| {
                matches!(step, Step::Run { argv, .. }
                    if argv.first().map(String::as_str) == Some(bin)
                        && argv.get(1).map(String::as_str) == Some("selftest"))
            }),
            "nothing runs the broker's own selftest"
        );
    }
}
