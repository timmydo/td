use crate::ladder::{post_bootstrap_path, unpack_into, POST_BOOTSTRAP_SH};
use crate::types::{Recipe, Step};

// A build-only static libcap surface for Codex's vendored Bubblewrap. The
// recipe deliberately omits libpsx, Go bindings, tools, shared libraries, and
// file-capability installation. Bubblewrap consumes the archive at its final
// link boundary, where td's runtime debug-companion policy is enforced.
pub fn recipe() -> Recipe {
    let gcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let binutils = "{in:binutils-x86-64-self}/bin";
    let glibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let path = format!("{{root}}/wb:{{tools}}:{binutils}:{}", post_bootstrap_path());
    let mut steps = unpack_into("libcap-x86-64-source", "{src}");

    steps.push(Step::ToolFarm {
        links: [
            "cat", "chmod", "cp", "echo", "false", "grep", "ln", "mkdir", "printf", "pwd", "rm",
            "sed", "test", "true", "which",
        ]
        .iter()
        .map(|name| ((*name).into(), "{in:busybox-x86-64}/bin/busybox".into()))
        .collect(),
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{POST_BOOTSTRAP_SH}\n\
             unset C_INCLUDE_PATH CPLUS_INCLUDE_PATH LIBRARY_PATH\n\
             gcc_include=$(\"{gcc}\" -print-file-name=include) || exit 1\n\
             test -n \"$gcc_include\" || exit 1\n\
             exec \"{gcc}\" -static -nostdinc -isystem \"$gcc_include\" \
             -isystem \"{glibc}/include\" \
             -B\"{binutils}/\" -B\"{glibc}/lib\" -L\"{glibc}/lib\" \
             -static-libgcc \"$@\" -fno-omit-frame-pointer -g1 \
             -ffile-prefix-map=\"{{root}}\"=/td-build-root \
             -ffile-prefix-map=\"{{src}}\"=/td-build \
             -ffile-prefix-map=\"{{in:gcc-x86-64-self}}\"=/td-build/input/gcc \
             -ffile-prefix-map=\"{{in:gcc-x86-64-self}}/stage/td/store/gcc-14.3.0-x86_64-self\"=/td-build/input/gcc-runtime \
             -ffile-prefix-map=\"$gcc_include\"=/td-build/input/gcc-includes \
             -ffile-prefix-map=\"{{in:binutils-x86-64-self}}\"=/td-build/input/binutils \
             -ffile-prefix-map=\"{{in:glibc-x86-64}}\"=/td-build/input/glibc \
             -ffile-prefix-map=\"{glibc}\"=/td-build/input/glibc-runtime \
             -Wl,--build-id=sha1\n"
        ),
        exec: true,
    });
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-x86-64-self}/bin/make",
                "-j{jobs}",
                "-C",
                "libcap",
                "libcap.a",
                "SHARED=no",
                "PTHREADS=no",
                "USE_GPERF=no",
                "CC={root}/wb/cc",
                "BUILD_CC={root}/wb/cc",
                "AR={in:binutils-x86-64-self}/bin/ar",
                "RANLIB={in:binutils-x86-64-self}/bin/ranlib",
                "BUILD_SED=sed",
                "BUILD_GREP=grep",
                "SHELL={in:busybox-x86-64}/bin/sh",
            ],
        )
        .env("PATH", &path)
        .env("SHELL", POST_BOOTSTRAP_SH)
        .env("MAKEFLAGS", "")
        .env("MFLAGS", "")
        .env("GNUMAKEFLAGS", "")
        .env("MAKELEVEL", "")
        .env("SOURCE_DATE_EPOCH", "1"),
    );

    for dir in ["{out}/lib", "{out}/include/sys", "{out}/include/linux"] {
        steps.push(Step::MkDir { path: dir.into() });
    }
    steps.push(Step::CopyFiles {
        files: vec!["{src}/libcap/libcap.a".into()],
        dest: "{out}/lib".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/libcap/include/sys/capability.h".into()],
        dest: "{out}/include/sys".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/libcap/include/uapi/linux/capability.h".into()],
        dest: "{out}/include/linux".into(),
    });
    steps.push(Step::Require {
        paths: vec![
            "{out}/lib/libcap.a".into(),
            "{out}/include/sys/capability.h".into(),
            "{out}/include/linux/capability.h".into(),
        ],
        exec: false,
    });

    // Link and run against the installed archive and headers, rather than the
    // source tree, so this is a behavioral check of the exact realized output.
    steps.push(Step::WriteFile {
        path: "{root}/libcap-smoke.c".into(),
        content: "#include <string.h>\n\
                  #include <sys/capability.h>\n\
                  int main(void) {\n\
                    cap_value_t value = -1;\n\
                    if (cap_from_name(\"cap_chown\", &value) != 0 || value != CAP_CHOWN) return 1;\n\
                    char *name = cap_to_name(CAP_NET_BIND_SERVICE);\n\
                    if (name == 0) return 2;\n\
                    int ok = strcmp(name, \"cap_net_bind_service\") == 0;\n\
                    if (cap_free(name) != 0) return 3;\n\
                    return ok ? 0 : 4;\n\
                  }\n"
            .into(),
        exec: false,
    });
    steps.push(Step::run(
        "{root}",
        &[
            "{root}/wb/cc",
            "-O2",
            "-I{out}/include",
            "{root}/libcap-smoke.c",
            "{out}/lib/libcap.a",
            "-o",
            "{root}/libcap-smoke",
        ],
    ));
    steps.push(Step::run("{root}", &["{root}/libcap-smoke"]));

    Recipe::mesboot("libcap-x86-64", "2.78")
        .source_input("libcap-x86-64-source")
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
    fn static_library_build_has_the_exact_post_bootstrap_inputs() {
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
    }

    #[test]
    fn compiler_policy_follows_package_arguments_and_output_is_exercised() {
        let steps = recipe().steps.expect("libcap steps");
        let wrapper = steps
            .iter()
            .find_map(|step| match step {
                Step::WriteFile { path, content, .. } if path == "{root}/wb/cc" => Some(content),
                _ => None,
            })
            .expect("compiler wrapper");
        assert!(
            wrapper.find("\"$@\"").expect("package arguments")
                < wrapper
                    .rfind("-fno-omit-frame-pointer")
                    .expect("target policy")
        );
        for required in [
            "unset C_INCLUDE_PATH CPLUS_INCLUDE_PATH LIBRARY_PATH",
            "-print-file-name=include",
            "-static",
            "-nostdinc",
            "-g1",
            "-ffile-prefix-map=\"{root}\"=/td-build-root",
            "-ffile-prefix-map=\"{src}\"=/td-build",
            "-ffile-prefix-map=\"{in:gcc-x86-64-self}\"=/td-build/input/gcc",
            "-ffile-prefix-map=\"{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self\"=/td-build/input/gcc-runtime",
            "-ffile-prefix-map=\"$gcc_include\"=/td-build/input/gcc-includes",
            "-ffile-prefix-map=\"{in:binutils-x86-64-self}\"=/td-build/input/binutils",
            "-ffile-prefix-map=\"{in:glibc-x86-64}\"=/td-build/input/glibc",
            "-ffile-prefix-map=\"{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64\"=/td-build/input/glibc-runtime",
            "-Wl,--build-id=sha1",
        ] {
            assert!(
                wrapper.contains(required),
                "missing target policy {required}"
            );
        }
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::Run { argv, .. }
                if argv.first().map(String::as_str) == Some("{root}/libcap-smoke")
        )));
    }
}
