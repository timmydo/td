use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

const TD_SSH: &str = "{in:td-ssh}/bin/td-ssh";
const TD_SSHD: &str = "{in:td-ssh}/bin/td-sshd";
const TD_KEYGEN: &str = "{in:td-ssh}/bin/td-ssh-keygen";

/// The realized-output check for td-ssh. A build has no loopback, so the recipe
/// itself proves only the binaries' static shape and that each entry runs; the
/// check's runner then drives real logins between these binaries and stock
/// OpenSSH on the check sandbox's loopback (checks/ssh_interop.rs), which is the
/// proof a wrong KEX or AEAD cannot pass. `openssh-x86-64` is an input only so
/// that it can be that peer, `td-sh` so that a logged-in account has a shell, and
/// `td-ssh-interop` is the driver.
pub fn recipe() -> Recipe {
    Recipe::mesboot("td-ssh-test", "1.0")
        .native_inputs(&["td-ssh", "td-ssh-interop", "openssh-x86-64", "td-sh"])
        .steps(vec![
            Step::Require {
                paths: vec![TD_SSH.into(), TD_SSHD.into(), TD_KEYGEN.into()],
                exec: true,
            },
            Step::assert_static(&[TD_SSH, TD_SSHD, TD_KEYGEN]),
            Step::run("{root}", &[TD_SSH, "-Q", "kex"]),
            Step::run("{root}", &[TD_SSH, "-V"]),
            Step::MkDir {
                path: "{out}".into(),
            },
            Step::WriteFile {
                path: "{out}/result".into(),
                content: "PASS: td-ssh, td-sshd and td-ssh-keygen are static target executables\n"
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
echo ">> recipe-check td-ssh-test: real logins between td-ssh and stock OpenSSH, both ways, over the check's loopback"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-ssh-test 1
"#,
        )
        .with_runner(CheckRunner::SshInterop)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_peer_and_the_shell_are_declared_and_the_interop_runner_is_named() {
        let recipe = recipe();
        assert_eq!(
            recipe.native_inputs.as_deref(),
            Some(
                ["td-ssh", "td-ssh-interop", "openssh-x86-64", "td-sh"]
                    .map(String::from)
                    .as_slice()
            )
        );
        let checks = recipe.checks.expect("checks");
        assert_eq!(checks.len(), 1);
        assert!(matches!(checks[0].runner, Some(CheckRunner::SshInterop)));
    }
}
