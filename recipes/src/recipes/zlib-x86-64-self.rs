use crate::ladder::{post_bootstrap_path, unpack_into, POST_BOOTSTRAP_SH};
use crate::types::{Recipe, Step};

// Final-toolchain zlib for source-built userland. The earlier zlib-x86-64
// output is deliberately a bootstrap-only shared library for the downloaded
// Rust snapshot; this rung instead exports a profiled static archive and public
// headers for curl and Git.
pub fn recipe() -> Recipe {
    let sgcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let sbin = "{in:binutils-x86-64-self}/bin";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let path = format!("{{root}}/wb:{{tools}}:{sbin}:{}", post_bootstrap_path());

    let mut steps = unpack_into("zlib-x86-64-self-source", "{src}");
    steps.push(Step::ToolFarm {
        links: [
            "awk", "basename", "cat", "chmod", "cmp", "cp", "cut", "date", "diff", "dirname",
            "echo", "env", "expr", "false", "find", "grep", "head", "install", "ln", "ls", "mkdir",
            "mktemp", "mv", "printf", "pwd", "rm", "rmdir", "sed", "sort", "tail", "tee", "test",
            "touch", "tr", "true", "uname", "wc", "which", "xargs",
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
             exec \"{sgcc}\" -static -isystem \"{xglibc}/include\" \
             -B\"{sbin}/\" -B\"{xglibc}/lib\" \
             -L\"{xglibc}/lib\" -static-libgcc \"$@\" \
             -fno-omit-frame-pointer -g1 \
             -ffile-prefix-map={{root}}=/td-build-root \
             -ffile-prefix-map={{src}}=/td-build -Wl,--build-id=sha1\n"
        ),
        exec: true,
    });
    steps.push(
        Step::run(
            "{src}",
            &[
                POST_BOOTSTRAP_SH,
                "./configure",
                "--prefix={out}",
                "--static",
            ],
        )
        .env("PATH", &path)
        .env("CC", "{root}/wb/cc")
        .env("CHOST", "x86_64-pc-linux-gnu")
        .env("AR", "{in:binutils-x86-64-self}/bin/ar")
        .env("RANLIB", "{in:binutils-x86-64-self}/bin/ranlib")
        .env("CONFIG_SHELL", POST_BOOTSTRAP_SH)
        .env("SHELL", POST_BOOTSTRAP_SH)
        .env("CFLAGS", "-O2")
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-x86-64-self}/bin/make",
                "-j{jobs}",
                "libz.a",
                &format!("SHELL={POST_BOOTSTRAP_SH}"),
            ],
        )
        .env("PATH", &path)
        .env("CC", "{root}/wb/cc")
        .env("AR", "{in:binutils-x86-64-self}/bin/ar")
        .env("RANLIB", "{in:binutils-x86-64-self}/bin/ranlib")
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::MkDir {
        path: "{out}/lib".into(),
    });
    steps.push(Step::MkDir {
        path: "{out}/include".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/libz.a".into()],
        dest: "{out}/lib".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/zlib.h".into(), "{src}/zconf.h".into()],
        dest: "{out}/include".into(),
    });
    // Consumers use explicit include/archive paths, so do not ship the generated
    // pkg-config file or run zlib's broader install target. The configured exact
    // prefix still keeps generated metadata truthful if a later consumer reviews
    // and adds that interface.
    steps.push(Step::Require {
        paths: vec![
            "{out}/lib/libz.a".into(),
            "{out}/include/zlib.h".into(),
            "{out}/include/zconf.h".into(),
        ],
        exec: false,
    });

    Recipe::mesboot("zlib-x86-64-self", "1.3.1")
        .source_input("zlib-x86-64-source")
        .native_inputs(&[
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "make-x86-64-self",
            "busybox-x86-64",
        ])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::recipe;
    use crate::types::Step;

    #[test]
    fn static_build_uses_only_post_bootstrap_inputs() {
        let recipe = recipe();
        assert_eq!(
            recipe.native_inputs.as_deref(),
            Some(
                [
                    "gcc-x86-64-self",
                    "binutils-x86-64-self",
                    "glibc-x86-64",
                    "make-x86-64-self",
                    "busybox-x86-64",
                ]
                .map(str::to_string)
                .as_slice()
            )
        );
        assert!(recipe.inputs.is_none());

        let steps = recipe.steps.expect("zlib steps");
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::Unpack { input, .. } if input == "{in:zlib-x86-64-self-source}"
        )));
        let configure = steps.iter().find_map(|step| match step {
            Step::Run { argv, .. } if argv.iter().any(|arg| arg == "./configure") => Some(argv),
            _ => None,
        });
        let configure = configure.expect("configure step");
        assert!(configure.iter().any(|arg| arg == "--prefix={out}"));
        assert!(configure.iter().any(|arg| arg == "--static"));

        let make = steps.iter().find_map(|step| match step {
            Step::Run { argv, .. } if argv.first().is_some_and(|arg| arg.contains("/bin/make")) => {
                argv.first().map(String::as_str)
            }
            _ => None,
        });
        assert_eq!(make, Some("{in:make-x86-64-self}/bin/make"));
    }

    #[test]
    fn compiler_wrapper_keeps_the_target_profile_policy_last() {
        let steps = recipe().steps.expect("zlib steps");
        let wrapper = steps
            .iter()
            .find_map(|step| match step {
                Step::WriteFile { path, content, .. } if path == "{root}/wb/cc" => Some(content),
                _ => None,
            })
            .expect("compiler wrapper");
        let package_args = wrapper.find("\"$@\"").expect("package arguments");
        let profile_policy = wrapper
            .rfind("-fno-omit-frame-pointer")
            .expect("frame-pointer policy");
        assert!(package_args < profile_policy);
        for required in [
            "-g1",
            "-ffile-prefix-map={root}=/td-build-root",
            "-ffile-prefix-map={src}=/td-build",
            "-Wl,--build-id=sha1",
        ] {
            assert!(
                wrapper.contains(required),
                "missing target policy {required}"
            );
        }
    }
}
