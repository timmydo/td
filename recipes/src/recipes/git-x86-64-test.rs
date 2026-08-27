use crate::ladder::{post_bootstrap_path, POST_BOOTSTRAP_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

pub fn recipe() -> Recipe {
    let git = "{in:git-x86-64}";
    let curl_test = "{in:curl-x86-64-test}";
    let openssh = "{in:openssh-x86-64}";
    let openssh_test = "{in:openssh-x86-64-test}";
    let readelf = "{in:binutils-x86-64-self}/bin/readelf";
    let path = format!(
        "{git}/bin:{git}/libexec/git-core:{openssh}/bin:{{tools}}:{}",
        post_bootstrap_path()
    );
    let mut steps = vec![
        Step::Require {
            paths: vec![
                format!("{git}/bin/git"),
                format!("{git}/bin/git-receive-pack"),
                format!("{git}/bin/git-upload-archive"),
                format!("{git}/bin/git-upload-pack"),
                format!("{git}/libexec/git-core/git-remote-http"),
                format!("{git}/libexec/git-core/git-remote-https"),
                format!("{git}/libexec/git-core/git-sh-i18n--envsubst"),
                format!("{openssh}/bin/ssh"),
            ],
            exec: true,
        },
        Step::Require {
            paths: vec![
                format!("{curl_test}/result"),
                format!("{openssh_test}/result"),
            ],
            exec: false,
        },
        Step::Require {
            paths: vec![
                format!("{git}/lib/debug/bin/git.debug"),
                format!("{git}/lib/debug/libexec/git-core/git-remote-http.debug"),
                format!("{git}/lib/debug/libexec/git-core/git-sh-i18n--envsubst.debug"),
            ],
            exec: false,
        },
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "grep -Fq 'PASS: curl 8.21.0 performs verified local-socket HTTPS' '{curl_test}/result' || \
                         {{ echo 'Git transport prerequisite did not pass its verified TLS oracle' >&2; exit 1; }}; \
                     grep -Fq 'PASS: OpenSSH Portable 10.5p1 provides the bounded ssh/sshd/ssh-keygen profile' '{openssh_test}/result' || \
                         {{ echo 'Git SSH prerequisite did not pass the OpenSSH package oracle' >&2; exit 1; }}; \
                     for pair in 'git.c|{git}/lib/debug/bin/git.debug' 'remote-curl.c|{git}/lib/debug/libexec/git-core/git-remote-http.debug' 'sh-i18n--envsubst.c|{git}/lib/debug/libexec/git-core/git-sh-i18n--envsubst.debug'; do \
                         source=${{pair%%|*}}; debug=${{pair#*|}}; \
                         grep -a -Fq \"$source\" \"$debug\" || {{ echo \"Git debug companion omits $source\" >&2; exit 1; }}; \
                         '{readelf}' --debug-dump=info \"$debug\" 2>/dev/null | \
                             awk '/DW_AT_comp_dir/ {{ seen=1; if ($0 !~ /: \\/td-build(\\/[^[:space:]]+)?$/) {{ print \"noncanonical Git closure comp_dir: \" $0 > \"/dev/stderr\"; bad=1 }} }} END {{ if (!seen || bad) exit 1 }}' || exit 1; \
                         for forbidden in '/gnu/store' '/home/' '/tmp/' '/.td/' 'guix-build' '/td-input/'; do \
                             if '{readelf}' --debug-dump=info,rawline \"$debug\" 2>/dev/null | grep -Fq \"$forbidden\"; then echo \"Git $source exposes forbidden debug path $forbidden\" >&2; exit 1; fi; \
                         done; \
                     done; \
                     for binary in '{git}/bin/git' '{git}/libexec/git-core/git-remote-http' '{git}/libexec/git-core/git-sh-i18n--envsubst'; do \
                         header=$('{readelf}' -h \"$binary\") || exit 1; \
                         printf '%s\\n' \"$header\" | grep -Fq 'Class:                             ELF64' || exit 1; \
                         printf '%s\\n' \"$header\" | grep -Fq 'Machine:                           Advanced Micro Devices X86-64' || exit 1; \
                         program=$('{readelf}' -l \"$binary\") || exit 1; \
                         printf '%s\\n' \"$program\" | grep -Fq '{{in:glibc-x86-64}}/stage/td/store/glibc-2.41-x86_64/lib/ld-linux-x86-64.so.2' || {{ echo 'Git does not use the final glibc interpreter' >&2; exit 1; }}; \
                         dynamic=$('{readelf}' -d \"$binary\") || exit 1; \
                         needed=$(printf '%s\\n' \"$dynamic\" | sed -n 's/.*Shared library: \\[\\(.*\\)\\]/\\1/p') || exit 1; \
                         test \"$needed\" = libc.so.6 || {{ echo \"Git has an unexpected dynamic dependency set: $needed\" >&2; exit 1; }}; \
                         runpath=$(printf '%s\\n' \"$dynamic\" | sed -n 's/.*Library runpath: \\[\\(.*\\)\\]/\\1/p') || exit 1; \
                         test \"$runpath\" = '{{in:glibc-x86-64}}/stage/td/store/glibc-2.41-x86_64/lib' || {{ echo \"Git has an unexpected RUNPATH: $runpath\" >&2; exit 1; }}; \
                         if printf '%s\\n' \"$dynamic\" | grep -Fq '(RPATH)'; then echo 'Git carries legacy RPATH in addition to RUNPATH' >&2; exit 1; fi; \
                         for forbidden in '/gnu/store' '/td-input/'; do \
                             if grep -a -Fq \"$forbidden\" \"$binary\"; then echo \"Git binary retains forbidden path $forbidden\" >&2; exit 1; fi; \
                         done; \
                     done"
                ),
            ],
        )
        .env("PATH", &path),
        Step::MkDir {
            path: "{root}/home".into(),
        },
        Step::MkDir {
            path: "{root}/work".into(),
        },
    ];
    steps.push(
        Step::run(
            "{root}/work",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                "git init -b main repo || exit 1; \
                 cd repo || exit 1; \
                 test -f .git/info/exclude || exit 1; \
                 test -f .git/hooks/pre-commit.sample || exit 1; \
                 git config user.name 'td test' || exit 1; \
                 git config user.email td@example.invalid || exit 1; \
                 printf '%s\\n' one > tracked || exit 1; \
                 git add tracked || exit 1; \
                 git commit -m one || exit 1; \
                 git switch -c feature || exit 1; \
                 printf '%s\\n' two >> tracked || exit 1; \
                 git commit -am two || exit 1; \
                 git switch main || exit 1; \
                 git merge --no-edit feature || exit 1; \
                 printf '%s\\n' scratch >> tracked || exit 1; \
                 git stash push -m scratch || exit 1; \
                 test -z \"$(git status --porcelain)\" || exit 1; \
                 git stash pop || exit 1; \
                 git reset --hard HEAD || exit 1; \
                 git tag v1 || exit 1; \
                 git gc || exit 1; \
                 git fsck --strict || exit 1; \
                 git archive --format=tar HEAD > archive.tar || exit 1; \
                 test -s archive.tar || exit 1; \
                 test \"$(git rev-list --count HEAD)\" = 2 || exit 1; \
                 test \"$(git describe --tags --exact-match HEAD)\" = v1 || exit 1; \
                 cd .. || exit 1; \
                 git init --bare -b main origin || exit 1; \
                 git-upload-pack --stateless-rpc --advertise-refs repo > upload.refs || exit 1; \
                 test -s upload.refs || exit 1; \
                 git-receive-pack --stateless-rpc --advertise-refs origin > receive.refs || exit 1; \
                 test -s receive.refs || exit 1; \
                 printf '001aargument --format=tar\\n0012argument HEAD\\n0000' | git-upload-archive repo > remote.archive || exit 1; \
                 test -s remote.archive || exit 1; \
                 if GIT_TRACE=1 GIT_TERMINAL_PROMPT=0 git ls-remote https://127.0.0.1:9/td-no-service >https-helper.log 2>&1; then exit 1; fi; \
                 grep -Fq 'git-remote-https' https-helper.log || exit 1; \
                 if GIT_TRACE=1 GIT_TERMINAL_PROMPT=0 GIT_SSH_COMMAND='{in:openssh-x86-64}/bin/ssh -F /dev/null -o BatchMode=yes' git ls-remote ssh://127.0.0.1:9/td-no-service >ssh-helper.log 2>&1; then exit 1; fi; \
                 grep -Fq '{in:openssh-x86-64}/bin/ssh' ssh-helper.log || exit 1; \
                 cd repo || exit 1; \
                 build=$(git version --build-options) || exit 1; \
                 printf '%s\\n' \"$build\" | grep -Fq 'git version 2.55.0' || exit 1; \
                 printf '%s\\n' \"$build\" | grep -Fq 'shell-path: /bin/sh' || exit 1; \
                 test \"$(git --exec-path)\" = '{in:git-x86-64}/libexec/git-core' || exit 1",
            ],
        )
        .env("PATH", &path)
        .env("HOME", "{root}/home")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("LC_ALL", "C"),
    );
    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content: "PASS: Git 2.55.0 local and service workflows, HTTPS helper dispatch with verified curl TLS, and SSH dispatch through verified OpenSSH; the system boot oracle completes clone/push/reclone over a real loopback sshd\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("git-x86-64-test", "1.0")
        .native_inputs(&[
            "git-x86-64",
            "curl-x86-64-test",
            "openssh-x86-64",
            "openssh-x86-64-test",
            "glibc-x86-64",
            "binutils-x86-64-self",
            "busybox-x86-64",
        ])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check git-x86-64-test: build Git, exercise local service workflows plus HTTPS/OpenSSH dispatch, and require both transport oracles"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run git-x86-64-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::recipe;

    #[test]
    fn validation_composes_git_with_both_verified_transport_oracles() {
        let recipe = recipe();
        assert_eq!(
            recipe.native_inputs.as_deref(),
            Some(
                [
                    "git-x86-64",
                    "curl-x86-64-test",
                    "openssh-x86-64",
                    "openssh-x86-64-test",
                    "glibc-x86-64",
                    "binutils-x86-64-self",
                    "busybox-x86-64",
                ]
                .map(str::to_string)
                .as_slice()
            )
        );
        assert!(recipe.inputs.is_none());
    }
}
