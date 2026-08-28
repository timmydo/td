use crate::ladder::{debug_line_source_root_check, post_bootstrap_path, POST_BOOTSTRAP_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

pub fn recipe() -> Recipe {
    let make = "{in:make-x86-64-self}/bin/make";
    let debug = "{in:make-x86-64-self}/lib/debug/bin/make.debug";
    let readelf = "{in:binutils-x86-64-self}/bin/readelf";
    let mut steps = vec![
        Step::Require {
            paths: vec![make.into()],
            exec: true,
        },
        Step::Require {
            paths: vec![debug.into()],
            exec: false,
        },
        Step::MkDir {
            path: "{root}/test".into(),
        },
        Step::WriteFile {
            path: "{root}/test/Makefile".into(),
            content: "V := world\nall: greeting.txt\ngreeting.txt:\n\tprintf 'hello, %s\\n' '$(V)' > $@\nfeatures:\n\t@printf '%s\\n' '$(.FEATURES)'\n.PHONY: all features\n"
                .into(),
            exec: false,
        },
    ];
    steps.push(debug_line_source_root_check(
        readelf,
        debug,
        "main.c",
        "/td-build",
    ));
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "if grep -a -Fq 'guix-build' '{debug}'; then \
                         echo 'make debug companion exposes a build scratch path' >&2; exit 1; \
                     fi"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );
    steps.push(
        Step::run(
            "{root}/test",
            &[
                make,
                "SHELL={in:busybox-x86-64}/bin/sh",
                "CONFIG_SHELL={in:busybox-x86-64}/bin/sh",
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );
    steps.push(
        Step::run(
            "{root}/test",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                "grep -qx -F 'hello, world' greeting.txt || { echo 'final make did not drive the test build' >&2; exit 1; }; \
                 v=$(\"{in:make-x86-64-self}/bin/make\" --version) || exit 1; \
                 printf '%s\\n' \"$v\" | grep -q -F 'GNU Make 4.4.1' || { echo 'final make version mismatch' >&2; exit 1; }; \
                 features=$(\"{in:make-x86-64-self}/bin/make\" -s features) || exit 1; \
                 case \" $features \" in *' load '*) echo 'final make still permits runtime plugins' >&2; exit 1;; esac; \
                 h=$(\"{in:binutils-x86-64-self}/bin/readelf\" -h \"{in:make-x86-64-self}/bin/make\") || exit 1; \
                 printf '%s\\n' \"$h\" | grep -q -F 'Class:                             ELF64' || { echo 'final make is not ELF64' >&2; exit 1; }; \
                 printf '%s\\n' \"$h\" | grep -q -F 'Machine:                           Advanced Micro Devices X86-64' || { echo 'final make is not x86-64' >&2; exit 1; }; \
                 if grep -q -a -F '/gnu/store' \"{in:make-x86-64-self}/bin/make\"; then echo 'final make retains a foreign store reference' >&2; exit 1; fi",
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );
    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content:
            "PASS: final GNU Make 4.4.1 is closed to plugins, carries canonical line information, and drove a real build\n"
                .into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("make-x86-64-self-test", "1.0")
        .native_inputs(&[
            "make-x86-64-self",
            "binutils-x86-64-self",
            "busybox-x86-64",
        ])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check make-x86-64-self-test: rebuild GNU Make with the final compiler, validate its debug companion, and drive a real build"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run make-x86-64-self-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::{recipe, Step};

    #[test]
    fn test_uses_only_post_bootstrap_inputs() {
        let recipe = recipe();
        assert_eq!(
            recipe
                .native_inputs
                .as_deref()
                .map(|inputs| inputs.iter().map(String::as_str).collect::<Vec<_>>()),
            Some(vec![
                "make-x86-64-self",
                "binutils-x86-64-self",
                "busybox-x86-64",
            ])
        );
        assert!(recipe.inputs.is_none());
        let line_validation = recipe
            .steps
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => argv.get(2),
                _ => None,
            })
            .find(|command| command.contains("--debug-dump=rawline"))
            .expect("line-only Make source-root validation");
        assert!(line_validation.contains("main.c"));
        assert!(line_validation.contains("/td-build"));
        assert!(!line_validation.contains("DW_AT_comp_dir"));
        assert!(recipe
            .steps
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => argv.get(2),
                _ => None,
            })
            .any(|command| command.contains("grep -a -Fq 'guix-build'")));
    }
}
