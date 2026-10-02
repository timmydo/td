use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

const BIN: &str = "{in:td-review}/bin/td-review";
const GIT: &str = "{in:git-x86-64}/bin/git";
const REPO: &str = "{root}/repo";

/// One step run with the image's git on PATH and no ambient identity or
/// configuration, as td-review's own land tests run it.
fn with_git(dir: &str, argv: &[&str]) -> Step {
    Step::run(dir, argv)
        .env(
            "PATH",
            "{in:git-x86-64}/bin:{in:git-x86-64}/libexec/git-core",
        )
        .env("HOME", "{root}/home")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "td test")
        .env("GIT_AUTHOR_EMAIL", "td@example.invalid")
        .env("GIT_COMMITTER_NAME", "td test")
        .env("GIT_COMMITTER_EMAIL", "td@example.invalid")
        .env("LC_ALL", "C")
}

fn write(path: &str, content: &str) -> Step {
    Step::WriteFile {
        path: path.into(),
        content: content.into(),
        exec: false,
    }
}

pub fn recipe() -> Recipe {
    // A base and a one-commit branch known only as a remote-tracking ref,
    // the shape a fetch leaves; landing it without --push needs no transport.
    let a = format!("{REPO}/a.txt");
    let readme = format!("{REPO}/README");
    let steps = vec![
        Step::Require {
            paths: vec![BIN.into(), GIT.into()],
            exec: true,
        },
        Step::assert_static(&[BIN]),
        Step::run("{root}", &[BIN, "--help"]),
        Step::MkDir {
            path: "{root}/home".into(),
        },
        with_git("{root}", &[GIT, "init", "-q", "-b", "main", REPO]),
        write(&readme, "base\n"),
        with_git(REPO, &[GIT, "add", "README"]),
        with_git(REPO, &[GIT, "commit", "-q", "-m", "base: initial commit"]),
        with_git(REPO, &[GIT, "switch", "-q", "-c", "feature"]),
        write(&a, "alpha\n"),
        with_git(REPO, &[GIT, "add", "a.txt"]),
        with_git(REPO, &[GIT, "commit", "-q", "-m", "feature: one step"]),
        with_git(
            REPO,
            &[GIT, "update-ref", "refs/remotes/origin/feature", "feature"],
        ),
        with_git(REPO, &[GIT, "switch", "-q", "main"]),
        with_git(REPO, &[GIT, "branch", "-q", "-D", "feature"]),
        with_git("{root}", &[BIN, "-C", REPO, "--list"]),
        with_git(
            "{root}",
            &[BIN, "-C", REPO, "--land", "origin/feature", "--yes"],
        ),
        with_git(
            REPO,
            &[
                GIT,
                "merge-base",
                "--is-ancestor",
                "refs/remotes/origin/feature",
                "main",
            ],
        ),
        Step::MkDir {
            path: "{out}".into(),
        },
        write(
            "{out}/result",
            "PASS: static target td-review executed its help, listed a branch and landed it with the image's git\n",
        ),
        Step::Require {
            paths: vec!["{out}/result".into()],
            exec: false,
        },
    ];
    Recipe::mesboot("td-review-test", "1.0")
        .native_inputs(&["td-review", "git-x86-64", "glibc-x86-64"])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            "exec \"$TD_RECIPE_EVAL\" check-run td-review-test 1\n",
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(steps: &[Step], want: &[&str]) -> usize {
        steps
            .iter()
            .position(|s| matches!(s, Step::Run { argv, .. } if *argv == want))
            .unwrap_or_else(|| panic!("no step runs {want:?}"))
    }

    #[test]
    fn companion_lists_and_lands_with_the_images_git() {
        let recipe = recipe();
        assert_eq!(
            recipe.native_inputs,
            Some(vec![
                "td-review".into(),
                "git-x86-64".into(),
                "glibc-x86-64".into()
            ])
        );
        let steps = recipe.steps.unwrap();
        let static_at = steps
            .iter()
            .position(
                |s| matches!(s, Step::AssertStatic { paths } if paths.iter().any(|p| p == BIN)),
            )
            .unwrap();
        let help = position(&steps, &[BIN, "--help"]);
        let tracked = position(
            &steps,
            &[GIT, "update-ref", "refs/remotes/origin/feature", "feature"],
        );
        let list = position(&steps, &[BIN, "-C", REPO, "--list"]);
        let land = position(
            &steps,
            &[BIN, "-C", REPO, "--land", "origin/feature", "--yes"],
        );
        let landed = position(
            &steps,
            &[
                GIT,
                "merge-base",
                "--is-ancestor",
                "refs/remotes/origin/feature",
                "main",
            ],
        );
        let result = steps
            .iter()
            .position(|s| matches!(s, Step::WriteFile { path, content, .. } if path == "{out}/result" && content.contains("landed")))
            .unwrap();
        assert!(static_at < help && help < tracked && tracked < list);
        assert!(list < land && land < landed && landed < result);
        // Every td-review run past --help finds git by PATH, as in the image.
        for at in [list, land] {
            let Some(Step::Run { env, .. }) = steps.get(at) else {
                panic!("step {at} is not a run");
            };
            assert!(env
                .iter()
                .any(|(k, v)| k == "PATH" && v.starts_with("{in:git-x86-64}/bin")));
        }
    }
}
