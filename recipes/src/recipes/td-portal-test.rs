use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// td-portal-test: shape and self-check validation of the target-built desktop
// portal. Per repo policy that a recipe tests its realized output, this re-proves
// against the shipped binary the two things the old hand-rolled td-portal recipe
// asserted inline before it became a generic `Recipe::rust` cargo build:
//   1. `{in:td-portal}/bin/td-portal` is a fully static target executable — the
//      NSS-free, loader-free shape the system tree's `/bin/td-portal` link needs,
//   2. its own `selftest` subcommand runs green on the target build.
// It is the portal's companion to td-ui-test, which makes the same split for the
// compositor's binaries. The behavioural portal proof (stand the service up under
// the broker and drive a real dialog) belongs to the qemu-boot spike outside this
// host-free sandbox, exactly as td-netd-test defers td-netd's bring-up.
pub fn recipe() -> Recipe {
    let bin = "{in:td-portal}/bin/td-portal";
    let steps = vec![
        Step::Require {
            paths: vec![bin.into()],
            exec: true,
        },
        Step::assert_static(&[bin]),
        Step::run("{root}", &[bin, "selftest"]),
        Step::MkDir {
            path: "{out}".into(),
        },
        Step::WriteFile {
            path: "{out}/result".into(),
            content: "PASS: td-portal is a static target executable whose own selftest runs green\n".into(),
            exec: false,
        },
        Step::Require {
            paths: vec!["{out}/result".into()],
            exec: false,
        },
    ];

    Recipe::mesboot("td-portal-test", "1.0")
        .native_inputs(&["td-portal"])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-portal-test: build-plan --auto builds td-portal (the supervised desktop portal, statically linked by the /td/store target Rust + native GCC/binutils/glibc toolchain) and asserts a static target executable whose own selftest runs green"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-portal-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every entry here is something a build would stay green without: a missing
    /// Require, a dropped static assertion, a selftest that is never run, or a
    /// result line claiming more than was checked.
    #[test]
    fn the_shipped_portal_is_required_static_and_selftested() {
        let steps = recipe().steps.expect("td-portal-test steps");
        let bin = "{in:td-portal}/bin/td-portal";
        assert!(
            steps.iter().any(|step| {
                matches!(step, Step::Require { paths, exec }
                    if *exec && paths.iter().any(|required| required == bin))
            }),
            "nothing requires td-portal"
        );
        assert!(
            steps.iter().any(|step| {
                matches!(step, Step::AssertStatic { paths }
                    if paths.iter().any(|asserted| asserted == bin))
            }),
            "nothing asserts td-portal is static"
        );
        assert!(
            steps.iter().any(|step| {
                matches!(step, Step::Run { argv, .. }
                    if argv.first().map(String::as_str) == Some(bin)
                        && argv.get(1).map(String::as_str) == Some("selftest"))
            }),
            "nothing runs the portal's own selftest"
        );
        assert!(
            steps.iter().any(|step| {
                matches!(step, Step::WriteFile { content, .. } if content.contains("td-portal"))
            }),
            "the result does not mention what it proved"
        );
    }

    /// The companion reads only the portal it proves; the target toolchain that
    /// builds that portal is td-portal's own native-input concern, not repeated
    /// here.
    #[test]
    fn the_companion_takes_only_the_binary_it_checks() {
        let recipe = recipe();
        assert_eq!(
            recipe.native_inputs,
            Some(vec!["td-portal".to_string()]),
            "td-portal-test reads only the binary under test"
        );
    }
}
