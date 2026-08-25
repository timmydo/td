use crate::ladder::{
    post_bootstrap_path, split_target_debug, unpack_into, unpack_keep_top, POST_BOOTSTRAP_SH,
};
use crate::types::{Recipe, Step};

// Rebuild GNU Make after the final compiler boundary. The preceding
// make-x86-64 is the one reviewed bootstrap driver: this recipe is its only
// post-bootstrap execution edge, and later packages consume this output
// instead. The frozen Linux 4.14 UAPI source is the same header set used to
// build the final compiler and glibc.
pub fn recipe() -> Recipe {
    let gcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let binutils = "{in:binutils-x86-64-self}/bin";
    let glibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let path = format!("{{root}}/wb:{{tools}}:{binutils}:{}", post_bootstrap_path());
    // The planner maps a recipe's source pin to its local `<name>-source`
    // input, even when the pin key is shared with an earlier recipe.
    let mut steps = unpack_into("make-x86-64-self-source", "{src}");

    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    // The installed BusyBox exposes only the post-bootstrap core symlinks.
    // Configure's grep stress probe also calls diff, so give every probe the
    // reviewed BusyBox multicall explicitly instead of broadening the image.
    steps.push(Step::ToolFarm {
        links: [
            "awk", "basename", "cat", "chmod", "cmp", "cp", "cut", "date", "diff", "dirname",
            "echo", "env", "expr", "false", "find", "grep", "head", "install", "ln", "ls",
            "mkdir", "mktemp", "mv", "printf", "pwd", "rm", "rmdir", "sed", "sort", "tail",
            "tee", "test", "touch", "tr", "true", "uname", "wc", "which", "xargs",
        ]
        .iter()
        .map(|name| ((*name).into(), "{in:busybox-x86-64}/bin/busybox".into()))
        .collect(),
    });
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: POST_BOOTSTRAP_SH.into(),
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{POST_BOOTSTRAP_SH}\n\
             exec \"{gcc}\" -static -idirafter \"{glibc}/include\" \
             -idirafter \"{{root}}/kh\" -B\"{binutils}/\" \
             -B\"{glibc}/lib\" -L\"{glibc}/lib\" \"$@\" \
             -fno-omit-frame-pointer -g1 \
             -ffile-prefix-map=\"{{root}}\"=/td-build-root \
             -ffile-prefix-map=\"{{src}}\"=/td-build \
             -Wl,--build-id=sha1\n"
        ),
        exec: true,
    });
    steps.push(
        Step::run(
            "{src}",
            &[
                POST_BOOTSTRAP_SH,
                "./configure",
                "--build=x86_64-pc-linux-gnu",
                "--host=x86_64-pc-linux-gnu",
                "--prefix={out}",
                "--disable-dependency-tracking",
                "--disable-nls",
                "--without-guile",
                "--disable-load",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", POST_BOOTSTRAP_SH)
        .env("SHELL", POST_BOOTSTRAP_SH)
        .env("CC", "{root}/wb/cc")
        .env("CC_FOR_BUILD", "{root}/wb/cc")
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-x86-64}/bin/make",
                "-j{jobs}",
                "SHELL={in:busybox-x86-64}/bin/sh",
                "CONFIG_SHELL={in:busybox-x86-64}/bin/sh",
                "MAKEINFO=true",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", POST_BOOTSTRAP_SH)
        .env("SHELL", POST_BOOTSTRAP_SH)
        .env("MAKEFLAGS", "")
        .env("MFLAGS", "")
        .env("GNUMAKEFLAGS", "")
        .env("MAKELEVEL", "")
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/make".into()],
        dest: "{out}/bin".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/bin/make".into()],
        exec: true,
    });
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/make"]));
    steps.push(
        Step::run("{out}", &["{out}/bin/make", "--version"]).env("PATH", &post_bootstrap_path()),
    );

    Recipe::mesboot("make-x86-64-self", "4.4.1")
        .source_input("make-x86-64-source")
        .native_inputs(&[
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "make-x86-64",
            "busybox-x86-64",
        ])
        .inputs(&["linux-headers-x86-64"])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::recipe;
    use crate::types::Step;

    #[test]
    fn bridge_inputs_and_execution_are_exact() {
        let recipe = recipe();
        assert_eq!(
            recipe
                .native_inputs
                .as_deref()
                .map(|inputs| inputs.iter().map(String::as_str).collect::<Vec<_>>()),
            Some(vec![
                "gcc-x86-64-self",
                "binutils-x86-64-self",
                "glibc-x86-64",
                "make-x86-64",
                "busybox-x86-64",
            ])
        );
        assert_eq!(
            recipe.inputs.as_deref(),
            Some(["linux-headers-x86-64".to_string()].as_slice())
        );
        let steps = recipe.steps.expect("make-x86-64-self steps");
        let bootstrap_make_runs: Vec<&Vec<String>> = steps
            .iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. }
                    if argv.first().map(String::as_str) == Some("{in:make-x86-64}/bin/make") =>
                {
                    Some(argv)
                }
                _ => None,
            })
            .collect();
        assert_eq!(bootstrap_make_runs.len(), 1);
        assert!(bootstrap_make_runs[0]
            .iter()
            .any(|arg| arg == "SHELL={in:busybox-x86-64}/bin/sh"));
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::Unpack { input, .. } if input == "{in:make-x86-64-self-source}"
        )));
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::Unpack { input, .. } if input == "{in:linux-headers-x86-64}"
        )));
    }

    #[test]
    fn compiler_policy_follows_package_arguments_and_output_is_split() {
        let steps = recipe().steps.expect("make-x86-64-self steps");
        let wrapper = steps
            .iter()
            .find_map(|step| match step {
                Step::WriteFile { path, content, .. } if path == "{root}/wb/cc" => Some(content),
                _ => None,
            })
            .expect("compiler wrapper");
        let package_args = wrapper.find("\"$@\"").expect("package arguments");
        let profile = wrapper
            .rfind("-fno-omit-frame-pointer")
            .expect("profile policy");
        assert!(package_args < profile);
        for required in [
            "-fno-omit-frame-pointer",
            "-g1",
            "-ffile-prefix-map=\"{root}\"=/td-build-root",
            "-ffile-prefix-map=\"{src}\"=/td-build",
            "-Wl,--build-id=sha1",
        ] {
            assert!(wrapper.contains(required), "missing target policy {required}");
        }
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::SplitDebugTree { root, objcopy }
                if root == "{out}"
                    && objcopy == "{in:binutils-x86-64-self}/bin/objcopy"
        )));
    }

    #[test]
    fn configure_uses_the_real_output_and_disables_runtime_plugins() {
        let steps = recipe().steps.expect("make-x86-64-self steps");
        let configure = steps
            .iter()
            .find_map(|step| match step {
                Step::Run { argv, .. } if argv.iter().any(|arg| arg == "./configure") => {
                    Some(argv)
                }
                _ => None,
            })
            .expect("configure step");
        assert!(configure.iter().any(|arg| arg == "--prefix={out}"));
        assert!(configure.iter().any(|arg| arg == "--disable-load"));
    }
}
